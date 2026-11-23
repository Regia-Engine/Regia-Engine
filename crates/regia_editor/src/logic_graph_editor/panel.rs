// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph panel (`Panel::LogicGraph`, spec 102 § 6): the egui-snarl canvas, the palette
//! (events, conditions, actions), the inspector of the selected node, the issues list and the live
//! view. The panel reads the document while drawing and collects [`PanelEdit`]s, applied after the
//! frame: every gesture becomes one [`super::edit`] call (one undo step; a node drag coalesces into
//! one step per node).

use std::collections::BTreeMap;

use egui_snarl::ui::{AnyPins, PinInfo, SnarlStyle, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, InPinId, OutPin, OutPinId, Snarl};
use regia_ai::logic::{Binding, LogicEvent};
use regia_ai::logic_edit::{
    CondMode, LogicGraphDoc, LogicIssue, LogicNodeKind, LogicOp, LogicSeverity, NodeId,
};
use regia_ai::state_tree::{Condition, Literal};
use regia_ecs::prelude::*;

use super::breakpoints::{BreakpointChange, BreakpointKind};
use super::variables::{VarScope, VarType, VariableEdit};
use super::watch::{ScopeSnapshot, WatchItem};
use super::{LiveView, LogicEditor, RuleActivity};

/// Each wired node's rule name and, for an action, its index in the chain.
pub type NodeOwners = BTreeMap<NodeId, (String, Option<usize>)>;

/// Panel UI state (not undoable).
#[derive(Default)]
pub struct PanelState {
    snarl: Option<Snarl<NodeId>>,
    /// `(session-independent revision, path)` the canvas was built for.
    snarl_rev: Option<u64>,
    /// The path typed for "New…".
    new_path: String,
    /// The path typed for "Open…".
    open_path: String,
    /// Draft RON text of inspector fields, by `(node, field)`.
    drafts: BTreeMap<(NodeId, String), String>,
    /// A new parameter name being typed for the selected action.
    new_key: String,
    /// The issues of the document revision they were computed for (validation compiles every rule,
    /// so it runs once per edit, not once per frame).
    issues: Option<(u64, std::sync::Arc<Vec<LogicIssue>>)>,
    /// Each wired node's rule and action index, for the revision they were computed for.
    owners: Option<(u64, std::sync::Arc<NodeOwners>)>,
    /// The palette's search text.
    search: String,
    /// The search text of a dropped wire's menu.
    wire_search: String,
    /// A node to frame on the canvas on the next draw (a Problems link, Go to).
    frame: Option<NodeId>,
    /// Bumped by each framing: the canvas id changes so egui-snarl fits the view again.
    frame_epoch: u64,
    /// The bottom-left tab.
    tab: BottomTab,
    /// The frame (`cumulative_frame_nr`) debugger edits were last taken in: egui may run the
    /// panel twice in one frame, and a breakpoint toggle must happen once.
    debug_frame: Option<u64>,
    /// Drafts of the Variables list (rename fields, new names, defaults), by field.
    var_drafts: BTreeMap<String, String>,
    /// The type a new local gets.
    new_local_type: Option<VarType>,
    /// The Watch list's add field.
    watch_draft: String,
    /// The evaluate box.
    eval_draft: String,
    /// The Nodes list beside the canvas: its keyboard route (FU-5, ADR-8628).
    keys: crate::canvas_keys::GraphKeys,
    /// The node the Nodes list's focus belongs to (the focus follows it when rows move).
    keys_node: Option<NodeId>,
}

/// The bottom-left tabs of the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BottomTab {
    /// The issues list.
    #[default]
    Issues,
    /// The Breakpoints list.
    Breakpoints,
    /// The Variables list.
    Variables,
    /// The Watch list.
    Watch,
    /// The last Find usages answer (w13 P-183).
    Usages,
}

impl BottomTab {
    const ALL: [(Self, &'static str); 5] = [
        (Self::Issues, "Issues"),
        (Self::Breakpoints, "Breakpoints"),
        (Self::Variables, "Variables"),
        (Self::Watch, "Watch"),
        (Self::Usages, "Usages"),
    ];
}

impl PanelState {
    /// Frames `node` on the canvas on the next draw.
    pub fn frame_node(&mut self, node: NodeId) {
        self.frame = Some(node);
    }

    /// Shows a bottom-left tab.
    pub fn show_tab(&mut self, tab: BottomTab) {
        self.tab = tab;
    }

    /// The bottom-left tab shown.
    #[must_use]
    pub fn tab(&self) -> BottomTab {
        self.tab
    }

    /// How many times the canvas was framed on a node (a Problems link, Go to, Enter in the Nodes
    /// list): each framing draws the canvas under a fresh id.
    #[must_use]
    pub fn frame_epoch(&self) -> u64 {
        self.frame_epoch
    }

    /// Whether the Nodes list's **Connect…** form is open.
    #[must_use]
    pub fn connect_open(&self) -> bool {
        self.keys.connect.is_some()
    }
}

/// The row (and accessible) name of node `id` in the Nodes list: its canvas title and its id, so
/// two nodes with one title (two `Always` conditions) still read apart.
#[must_use]
pub fn node_row_name(id: NodeId, kind: &LogicNodeKind) -> String {
    format!("{} (#{id})", node_title(kind))
}

/// The node a **Connect…** target names: a row name ([`node_row_name`]), an id (`#3` or `3`), or a
/// canvas title only one node has (case-insensitive). `None` when nothing or more than one node
/// matches.
#[must_use]
pub fn find_node(doc: &LogicGraphDoc, text: &str) -> Option<NodeId> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((&id, _)) = doc
        .nodes
        .iter()
        .find(|(id, n)| node_row_name(**id, &n.kind) == text)
    {
        return Some(id);
    }
    if let Ok(id) = text.trim_start_matches('#').parse::<NodeId>() {
        return doc.nodes.contains_key(&id).then_some(id);
    }
    let lower = text.to_lowercase();
    let mut titled = doc
        .nodes
        .iter()
        .filter(|(_, n)| node_title(&n.kind).to_lowercase() == lower)
        .map(|(id, _)| *id);
    match (titled.next(), titled.next()) {
        (Some(id), None) => Some(id),
        _ => None,
    }
}

/// The palette entries the Nodes list offers after node `from` (what its output may feed), as
/// `"Group: label"`, filtered by `filter` with the canvas search's scorer.
#[must_use]
pub fn kinds_after(
    doc: &LogicGraphDoc,
    tasks: &[String],
    from: NodeId,
    filter: &str,
) -> Vec<(String, LogicOp)> {
    let wire = Some(WireFrom::Output(from));
    let entries = if filter.trim().is_empty() {
        palette(tasks)
            .into_iter()
            .filter(|(group, _, _)| accepts(doc, group, wire))
            .collect()
    } else {
        palette_search(doc, tasks, filter, wire)
    };
    entries
        .into_iter()
        .map(|(group, label, op)| (format!("{group}: {label}"), op))
        .collect()
}

/// Each node a rule reaches: its rule name and, for an action, its index in the chain.
#[must_use]
pub fn node_owners(doc: &LogicGraphDoc) -> NodeOwners {
    let mut out = BTreeMap::new();
    for chain in doc.chains() {
        let Some(rule) = doc.rule_name(chain.event) else {
            continue;
        };
        out.insert(chain.event, (rule.to_string(), None));
        for id in &chain.conditions {
            out.insert(*id, (rule.to_string(), None));
        }
        for (i, id) in chain.actions.iter().enumerate() {
            out.insert(*id, (rule.to_string(), Some(i)));
        }
    }
    out
}

/// A change the panel asks for while drawing.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelEdit {
    /// Structural ops as one undo step (`label`, merge key, ops).
    Ops(String, Option<String>, Vec<LogicOp>),
    /// Select a node (or clear).
    Select(Option<NodeId>),
    /// Watch a play-world graph (entity bits).
    Watch(Option<u64>),
    /// Pause, resume or step the watched graph (applied by the next pump).
    Control(super::LiveControl),
    /// Save the graph.
    Save,
    /// Open a file.
    Open(String),
    /// Start a new graph.
    New(String),
    /// Change a breakpoint (ADR-8021).
    Breakpoint(BreakpointKind, String, BreakpointChange),
    /// Select a node and frame it on the canvas.
    GoToNode(NodeId),
    /// A Variables list change (one undo step).
    Variable(VariableEdit),
    /// Pin an entry to the Watch list.
    WatchAdd(WatchItem),
    /// Unpin a Watch list entry.
    WatchRemove(WatchItem),
    /// Turn the Watch list's Live refresh on or off.
    WatchLive(bool),
    /// Evaluate an expression against the paused graph.
    Evaluate(String),
    /// Find usages of an event, key or parameter across the project (w13 P-183).
    FindUsages(super::usages::UsageQuery),
    /// Open row `n` of the last Find usages answer.
    OpenUsage(usize),
}

/// The pin color of a node kind (events amber, conditions sky, actions green; Okabe-Ito hues).
#[must_use]
pub fn kind_color(kind: &LogicNodeKind) -> egui::Color32 {
    match kind {
        LogicNodeKind::Event { .. } => egui::Color32::from_rgb(230, 159, 0),
        LogicNodeKind::Condition { .. } => egui::Color32::from_rgb(86, 180, 233),
        LogicNodeKind::Action { .. } => egui::Color32::from_rgb(0, 158, 115),
    }
}

/// The header color of a rule's live activity.
#[must_use]
pub fn activity_color(activity: RuleActivity) -> egui::Color32 {
    match activity {
        RuleActivity::Fired => egui::Color32::from_rgb(40, 120, 70),
        RuleActivity::Matched => egui::Color32::from_rgb(120, 110, 40),
        RuleActivity::Rejected => egui::Color32::from_rgb(90, 90, 100),
        RuleActivity::Failed => egui::Color32::from_rgb(150, 50, 45),
    }
}

/// A short label for a node: the rule and event, the condition, or the task.
#[must_use]
pub fn node_title(kind: &LogicNodeKind) -> String {
    match kind {
        LogicNodeKind::Event { rule, event, .. } => format!("{rule}: {}", event_label(event)),
        LogicNodeKind::Condition { condition } => {
            let mode = match CondMode::of(condition) {
                CondMode::All => "all",
                CondMode::Any => "any",
                CondMode::Not => "not",
                CondMode::Leaf => "if",
            };
            format!("{mode} {}", short_ron(condition))
        }
        LogicNodeKind::Action { task, .. } => task.clone(),
    }
}

fn event_label(event: &LogicEvent) -> String {
    match event {
        LogicEvent::Start => "Start".into(),
        LogicEvent::Timer {
            every_ticks,
            jitter_ticks,
        } => format!("Timer {every_ticks}+{jitter_ticks}"),
        LogicEvent::BlackboardChanged { key } => format!("Changed {key}"),
        LogicEvent::EntityEvent(name) => format!("Event {name}"),
        LogicEvent::OnSequenceMarker { marker, .. } => format!("Marker {marker}"),
        LogicEvent::OnNetMessage { message } => format!("Net {message}"),
        LogicEvent::InputAction { action } => format!("Input {action}"),
    }
}

fn short_ron<T: serde::Serialize>(value: &T) -> String {
    let text = ron::to_string(value).unwrap_or_default();
    if text.chars().count() > 40 {
        let cut: String = text.chars().take(38).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// The canvas of a document: one snarl node per document node at its position, one wire per
/// document wire (output 0 → input 0).
#[must_use]
pub fn build_snarl(doc: &LogicGraphDoc) -> Snarl<NodeId> {
    let mut snarl = Snarl::new();
    let mut map = BTreeMap::new();
    for (id, node) in &doc.nodes {
        map.insert(*id, snarl.insert_node(egui::pos2(node.pos[0], node.pos[1]), *id));
    }
    for (from, to) in &doc.wires {
        if let (Some(&a), Some(&b)) = (map.get(from), map.get(to)) {
            snarl.connect(OutPinId { node: a, output: 0 }, InPinId { node: b, input: 0 });
        }
    }
    snarl
}

/// Reads a canvas back into `doc`: positions and wires come from the canvas, everything else from
/// `doc` (the inverse of [`build_snarl`]).
#[must_use]
pub fn doc_from_snarl(snarl: &Snarl<NodeId>, doc: &LogicGraphDoc) -> LogicGraphDoc {
    let mut out = doc.clone();
    for (_, pos, id) in snarl.nodes_pos_ids() {
        if let Some(node) = out.nodes.get_mut(id) {
            node.pos = [pos.x, pos.y];
        }
    }
    out.wires = snarl
        .wires()
        .filter_map(|(from, to)| Some((*snarl.get_node(from.node)?, *snarl.get_node(to.node)?)))
        .collect();
    out
}

/// The ops a canvas drag of a wire from `from` onto `to` makes: free both pins first (a pin takes
/// one wire; the new one replaces), then connect. Pin types are checked when the ops apply.
#[must_use]
pub fn connect_ops(doc: &LogicGraphDoc, from: NodeId, to: NodeId) -> Vec<LogicOp> {
    let mut ops = Vec::new();
    if let Some(&old) = doc.wires.get(&from) {
        ops.push(LogicOp::Disconnect { from, to: old });
    }
    if let Some(old) = doc.wire_into(to)
        && old != from
    {
        ops.push(LogicOp::Disconnect { from: old, to });
    }
    ops.push(LogicOp::Connect { from, to });
    ops
}

/// The palette: what "Add" offers, grouped events / conditions / actions.
#[must_use]
pub fn palette(tasks: &[String]) -> Vec<(&'static str, String, LogicOp)> {
    let event = |label: &str, event: LogicEvent| {
        (
            "Events",
            label.to_string(),
            LogicOp::AddEvent {
                rule: String::new(),
                event,
                priority: 0,
                cooldown_ticks: 0,
                id: None,
                at: None,
            },
        )
    };
    let cond = |label: &str, condition: Condition| {
        (
            "Conditions",
            label.to_string(),
            LogicOp::AddCondition {
                condition,
                id: None,
                at: None,
            },
        )
    };
    let mut out = vec![
        event("Start", LogicEvent::Start),
        event(
            "Timer",
            LogicEvent::Timer {
                every_ticks: 60,
                jitter_ticks: 0,
            },
        ),
        event(
            "Blackboard changed",
            LogicEvent::BlackboardChanged { key: "key".into() },
        ),
        event("Entity event", LogicEvent::EntityEvent("event".into())),
    ];
    // The match session's events (GR-7.6): an entity event named `match.<kind>`, routed by
    // `regia::logic_bridge` to graphs on a `MatchEventListener` and on the entities involved.
    for kind in regia_framework::rules::MATCH_EVENT_KINDS {
        out.push(event(
            format!("Match: {kind}").as_str(),
            LogicEvent::EntityEvent(format!("match.{kind}")),
        ));
    }
    out.extend([
        cond("Always", Condition::Always),
        cond("Key", Condition::Key("key".into())),
        cond("Has", Condition::Has("key".into())),
        cond(
            "Compare",
            Condition::Compare {
                key: "key".into(),
                op: regia_ai::state_tree::CompareOp::Gt,
                value: 0.0,
            },
        ),
        cond(
            "Text",
            Condition::Text {
                key: "key".into(),
                value: String::new(),
            },
        ),
        cond("All", Condition::All(Vec::new())),
        cond("Any", Condition::Any(Vec::new())),
        cond("Not", Condition::Not(Box::new(Condition::Always))),
    ]);
    for task in tasks {
        out.push((
            "Actions",
            task.clone(),
            LogicOp::AddAction {
                task: task.clone(),
                set: BTreeMap::new(),
                id: None,
                at: None,
            },
        ));
    }
    out
}

/// The pin a wire was dragged from when a search opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFrom {
    /// The output of this node: the new node takes the wire on its input.
    Output(NodeId),
    /// The input of this node: the new node drives it from its output.
    Input(NodeId),
}

/// Whether a palette group can take a wire dragged from `from` (`None`: any group). Chains run
/// event → conditions → actions: an event's or a condition's output feeds a condition or an
/// action, an action's output only an action; a condition's input is fed by an event or a
/// condition, an action's by anything.
#[must_use]
pub fn accepts(doc: &LogicGraphDoc, group: &str, from: Option<WireFrom>) -> bool {
    let kind = |id: NodeId| doc.nodes.get(&id).map(|n| &n.kind);
    match from {
        None => true,
        Some(WireFrom::Output(id)) => match kind(id) {
            Some(LogicNodeKind::Action { .. }) => group == "Actions",
            Some(_) => group != "Events",
            None => false,
        },
        Some(WireFrom::Input(id)) => match kind(id) {
            Some(LogicNodeKind::Condition { .. }) => group != "Actions",
            Some(LogicNodeKind::Action { .. }) => true,
            _ => false,
        },
    }
}

/// The palette entries matching `query` (fuzzy: the canvas search's scorer, over "group label"),
/// best first and ties in palette order, filtered to the groups that can take a wire dragged
/// from `from`.
#[must_use]
pub fn palette_search(
    doc: &LogicGraphDoc,
    tasks: &[String],
    query: &str,
    from: Option<WireFrom>,
) -> Vec<(&'static str, String, LogicOp)> {
    crate::graph_kit::search::rank(
        palette(tasks),
        query,
        |(group, _, _)| accepts(doc, group, from),
        |(group, label, _)| vec![label.clone(), format!("{group} {label}")],
    )
}

/// The ops that add palette entry `op` at `at`, wired to the pin a wire was dragged from (one undo
/// step): from an output the new node takes the wire on its input (the output's old wire goes), into
/// an input the new node drives it (the input's old wire goes).
#[must_use]
pub fn add_wired_ops(
    doc: &LogicGraphDoc,
    op: &LogicOp,
    at: [f32; 2],
    from: Option<WireFrom>,
) -> Vec<LogicOp> {
    let id = doc.next_id;
    let mut ops = Vec::new();
    match from {
        Some(WireFrom::Output(from)) => {
            if let Some(&old) = doc.wires.get(&from) {
                ops.push(LogicOp::Disconnect { from, to: old });
            }
            ops.push(placed(doc, op, at));
            ops.push(LogicOp::Connect { from, to: id });
        }
        Some(WireFrom::Input(to)) => {
            if let Some(old) = doc.wire_into(to) {
                ops.push(LogicOp::Disconnect { from: old, to });
            }
            ops.push(placed(doc, op, at));
            ops.push(LogicOp::Connect { from: id, to });
        }
        None => ops.push(placed(doc, op, at)),
    }
    ops
}

/// A palette op placed at `at`; a new event gets a free rule name.
fn placed(doc: &LogicGraphDoc, op: &LogicOp, at: [f32; 2]) -> LogicOp {
    let mut op = op.clone();
    match &mut op {
        LogicOp::AddEvent { rule, at: a, .. } => {
            let mut n = doc.rules.len() + 1;
            while doc.find_rule(&format!("rule_{n}")).is_some() {
                n += 1;
            }
            *rule = format!("rule_{n}");
            *a = Some(at);
        }
        LogicOp::AddCondition { at: a, .. } | LogicOp::AddAction { at: a, .. } => *a = Some(at),
        _ => {}
    }
    op
}

struct Viewer<'a> {
    doc: &'a LogicGraphDoc,
    tasks: &'a [String],
    errors: &'a BTreeMap<NodeId, String>,
    live: Option<&'a LiveView>,
    owners: &'a NodeOwners,
    selected: Option<NodeId>,
    edits: &'a mut Vec<PanelEdit>,
    /// The open graph's rule and event breakpoints (empty while it has no path).
    breakpoints: &'a super::breakpoints::GraphBreakpoints,
    /// The watched graph's scope snapshot (hover values).
    snapshot: Option<&'a ScopeSnapshot>,
    /// Pulsing wires.
    pulse: &'a super::watch::Pulse,
    /// The pulse color (theme token `accents.active`).
    pulse_color: egui::Color32,
    /// Whether debugger edits may be taken this pass (once per frame).
    fresh: bool,
    /// The search text of a dropped wire's menu.
    wire_search: &'a mut String,
}

/// `a` mixed toward `b` by `t` (0..=1), per channel.
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    egui::Color32::from_rgba_unmultiplied(
        ch(a.r(), b.r()),
        ch(a.g(), b.g()),
        ch(a.b(), b.b()),
        ch(a.a(), b.a()),
    )
}

impl Viewer<'_> {
    /// Pushes a debugger edit (breakpoints, watches): once per frame, see `PanelState::debug_frame`.
    fn debug_edit(&mut self, edit: PanelEdit) {
        if self.fresh {
            self.edits.push(edit);
        }
    }

    /// The wire color of a pin on the wire `from → to`: pulsing toward the theme's active accent.
    fn pulse_pin(&self, pin: PinInfo, from: NodeId, to: NodeId, base: egui::Color32) -> PinInfo {
        let strength = self.pulse.strength(from, to);
        if strength > 0.0 {
            let mut pin = pin;
            pin.wire_color = Some(mix(base, self.pulse_color, strength));
            pin
        } else {
            pin
        }
    }

    /// The menu of a dropped wire (or of a search started from a pin): a search box and the
    /// entries that can take the wire; picking one adds it wired (one undo step).
    fn wired_search_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, from: WireFrom) {
        ui.label("Add and connect");
        // The toolkit's search list: the field takes the focus when the menu opens, so Tab
        // reaches the hits (review 3616 L5).
        let list = crate::graph_kit::search::SearchList {
            hint: "Search",
            spoken: "Search nodes to connect",
            max_hits: usize::MAX,
            max_height: 240.0,
            empty: None,
            width: None,
        };
        let (doc, tasks) = (self.doc, self.tasks);
        let hits = |query: &str| palette_search(doc, tasks, query, Some(from));
        let label = |(group, label, _): &(&'static str, String, LogicOp)| format!("{group}: {label}");
        if let Some((_, _, op)) = list.show(ui, self.wire_search, hits, label) {
            self.edits.push(PanelEdit::Ops(
                "add and connect".into(),
                None,
                add_wired_ops(self.doc, &op, [pos.x, pos.y], Some(from)),
            ));
            ui.close();
        }
    }

    fn kind(&self, id: NodeId) -> Option<&LogicNodeKind> {
        self.doc.nodes.get(&id).map(|n| &n.kind)
    }

    /// The rule a node belongs to, for live coloring.
    fn rule_of(&self, id: NodeId) -> Option<(&str, Option<usize>)> {
        self.owners
            .get(&id)
            .map(|(rule, action)| (rule.as_str(), *action))
    }
}

impl SnarlViewer<NodeId> for Viewer<'_> {
    fn title(&mut self, node: &NodeId) -> String {
        self.kind(*node).map_or_else(|| "?".into(), node_title)
    }

    fn header_frame(
        &mut self,
        default: egui::Frame,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        snarl: &Snarl<NodeId>,
    ) -> egui::Frame {
        let Some(id) = snarl.get_node(node).copied() else {
            return default;
        };
        let Some(live) = self.live else {
            return default;
        };
        let Some((rule, action)) = self.rule_of(id) else {
            return default;
        };
        let Some(activity) = live.activity.get(rule).copied() else {
            return default;
        };
        // An action node lights up only if the chain reached it.
        let reached = match action {
            Some(i) => live.actions_started.get(rule).is_some_and(|last| i <= *last),
            None => true,
        };
        if reached {
            default.fill(activity_color(activity))
        } else {
            default
        }
    }

    fn show_header(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<NodeId>,
    ) {
        let Some(id) = snarl.get_node(node).copied() else {
            return;
        };
        let title = self.title(&id);
        // The gutter dot of a rule head: a rule breakpoint (filled), disabled (ring), none (dim).
        if let Some(LogicNodeKind::Event { rule, .. }) = self.kind(id) {
            let rule = rule.clone();
            let bp = self.breakpoints.get(BreakpointKind::Rule, &rule).cloned();
            let (glyph, spoken) = match &bp {
                Some(b) if b.enabled => ("●", format!("Remove the breakpoint on rule {rule}")),
                Some(_) => ("◌", format!("Remove the disabled breakpoint on rule {rule}")),
                None => ("○", format!("Set a breakpoint on rule {rule}")),
            };
            if crate::a11y_audit::glyph_button(ui, glyph, &spoken).clicked() {
                let change = if bp.is_some() {
                    BreakpointChange::Remove
                } else {
                    BreakpointChange::Enable(true)
                };
                self.debug_edit(PanelEdit::Breakpoint(BreakpointKind::Rule, rule, change));
            }
        }
        let header = ui.selectable_label(self.selected == Some(id), format!("{title}  #{id}"));
        // A condition's live value while hovered (read from the paused or Live snapshot).
        let header = match (self.snapshot, self.kind(id)) {
            (Some(snapshot), Some(LogicNodeKind::Condition { .. })) => {
                let doc = self.doc;
                header.on_hover_ui(|ui| {
                    ui.label(super::watch::hover_value(Some(snapshot), doc, id).unwrap_or_default());
                })
            }
            _ => header,
        };
        if header.clicked() {
            self.edits.push(PanelEdit::Select(Some(id)));
        }
        if let Some(err) = self.errors.get(&id) {
            // Spoken as the error, in the theme's error color (graph_kit::badges).
            crate::graph_kit::badges::badge(ui, crate::graph_kit::badges::Severity::Error, err);
        }
    }

    fn inputs(&mut self, node: &NodeId) -> usize {
        match self.kind(*node) {
            Some(LogicNodeKind::Event { .. }) | None => 0,
            Some(_) => 1,
        }
    }

    fn outputs(&mut self, node: &NodeId) -> usize {
        usize::from(self.kind(*node).is_some())
    }

    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<NodeId>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        let id = snarl.get_node(pin.id.node).copied().unwrap_or(0);
        ui.label("in");
        let color = self.kind(id).map_or(egui::Color32::GRAY, kind_color);
        let info = PinInfo::circle().with_fill(color);
        match self.doc.wire_into(id) {
            Some(from) => self.pulse_pin(info, from, id, color),
            None => info,
        }
    }

    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<NodeId>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        let id = snarl.get_node(pin.id.node).copied().unwrap_or(0);
        ui.label("then");
        let color = self.kind(id).map_or(egui::Color32::GRAY, kind_color);
        let info = PinInfo::triangle().with_fill(color);
        match self.doc.wires.get(&id) {
            Some(&to) => self.pulse_pin(info, id, to, color),
            None => info,
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<NodeId>) {
        // The document is the source of truth: the canvas is rebuilt from it after the edit, so a
        // refused wire (pin type) never shows.
        if let (Some(a), Some(b)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) {
            self.edits.push(PanelEdit::Ops(
                "connect".into(),
                None,
                connect_ops(self.doc, a, b),
            ));
        }
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<NodeId>) {
        if let (Some(a), Some(b)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) {
            self.edits.push(PanelEdit::Ops(
                "disconnect".into(),
                None,
                vec![LogicOp::Disconnect { from: a, to: b }],
            ));
        }
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<NodeId>) {
        if let Some(a) = snarl.get_node(pin.id.node).copied()
            && let Some(&b) = self.doc.wires.get(&a)
        {
            self.edits.push(PanelEdit::Ops(
                "disconnect".into(),
                None,
                vec![LogicOp::Disconnect { from: a, to: b }],
            ));
        }
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<NodeId>) {
        if let Some(b) = snarl.get_node(pin.id.node).copied()
            && let Some(a) = self.doc.wire_into(b)
        {
            self.edits.push(PanelEdit::Ops(
                "disconnect".into(),
                None,
                vec![LogicOp::Disconnect { from: a, to: b }],
            ));
        }
    }

    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<NodeId>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, _snarl: &mut Snarl<NodeId>) {
        ui.label("Add node");
        let mut by_group: BTreeMap<&str, Vec<(String, LogicOp)>> = BTreeMap::new();
        for (group, label, op) in palette(self.tasks) {
            by_group.entry(group).or_default().push((label, op));
        }
        for (group, items) in by_group {
            ui.menu_button(group, |ui| {
                for (label, op) in items {
                    if ui.button(label).clicked() {
                        let op = placed(self.doc, &op, [pos.x, pos.y]);
                        self.edits.push(PanelEdit::Ops("add node".into(), None, vec![op]));
                        ui.close();
                    }
                }
            });
        }
    }

    fn has_dropped_wire_menu(&mut self, _src_pins: AnyPins, _snarl: &mut Snarl<NodeId>) -> bool {
        true
    }

    fn show_dropped_wire_menu(
        &mut self,
        pos: egui::Pos2,
        ui: &mut egui::Ui,
        src_pins: AnyPins,
        snarl: &mut Snarl<NodeId>,
    ) {
        // A wire dropped on empty canvas: a search over the palette filtered to what that pin may
        // connect to; choosing one adds it and wires it (one undo step).
        let from = match src_pins {
            AnyPins::Out(pins) => pins
                .first()
                .and_then(|p| snarl.get_node(p.node))
                .map(|id| WireFrom::Output(*id)),
            AnyPins::In(pins) => pins
                .first()
                .and_then(|p| snarl.get_node(p.node))
                .map(|id| WireFrom::Input(*id)),
        };
        if let Some(from) = from {
            self.wired_search_menu(pos, ui, from);
        }
    }

    fn has_node_menu(&mut self, _node: &NodeId) -> bool {
        true
    }

    fn show_node_menu(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<NodeId>,
    ) {
        let Some(id) = snarl.get_node(node).copied() else {
            return;
        };
        match self.kind(id).cloned() {
            Some(LogicNodeKind::Event { rule, event, .. }) => {
                let has_rule_bp = self.breakpoints.get(BreakpointKind::Rule, &rule).is_some();
                let rule_bp_label = if has_rule_bp {
                    "Remove rule breakpoint (F9)"
                } else {
                    "Break when this rule fires (F9)"
                };
                if ui.button(rule_bp_label).clicked() {
                    let change = if has_rule_bp {
                        BreakpointChange::Remove
                    } else {
                        BreakpointChange::Enable(true)
                    };
                    self.debug_edit(PanelEdit::Breakpoint(BreakpointKind::Rule, rule, change));
                    ui.close();
                }
                if let Some(name) = super::breakpoints::event_breakpoint_name(&event) {
                    let has = self.breakpoints.get(BreakpointKind::Event, &name).is_some();
                    let event_bp_label = if has {
                        format!("Remove event breakpoint on {name}")
                    } else {
                        format!("Break when {name} arrives")
                    };
                    if ui.button(event_bp_label).clicked() {
                        let change = if has {
                            BreakpointChange::Remove
                        } else {
                            BreakpointChange::Enable(true)
                        };
                        self.debug_edit(PanelEdit::Breakpoint(BreakpointKind::Event, name, change));
                        ui.close();
                    }
                }
            }
            Some(LogicNodeKind::Condition { .. }) if ui.button("Watch this condition").clicked() => {
                self.debug_edit(PanelEdit::WatchAdd(WatchItem::Condition(id)));
                ui.close();
            }
            _ => {}
        }
        // Find usages of each event and key the node names (w13 P-183).
        for query in super::usages::queries_for_node(self.doc, id) {
            if ui.button(query.label()).clicked() {
                self.edits.push(PanelEdit::FindUsages(query));
                ui.close();
            }
        }
        if ui.button("Delete").clicked() {
            self.edits.push(PanelEdit::Ops(
                "delete node".into(),
                None,
                vec![LogicOp::RemoveNode { node: id }],
            ));
            ui.close();
        }
    }
}

/// The issues as the panel lists them: `(code, severity, node, message)`.
#[must_use]
pub fn issue_rows(issues: &[LogicIssue]) -> Vec<(&'static str, &'static str, Option<NodeId>, String)> {
    issues
        .iter()
        .map(|i| {
            (
                i.code,
                if i.severity == LogicSeverity::Error {
                    "error"
                } else {
                    "warning"
                },
                i.node,
                i.message.clone(),
            )
        })
        .collect()
}

/// Draws the panel against `world` and applies its edits.
#[tracing::instrument(target = "regia_editor", skip_all, name = "editor.logic_graph_panel")]
pub fn logic_graph_panel(ui: &mut egui::Ui, world: &mut World) {
    let edits = draw(ui, world);
    let dragging = ui.input(|i| i.pointer.any_down());
    apply(world, edits, dragging);
}

/// Applies panel edits after a frame (also what the tests drive). A refused op leaves the document
/// and shows the reason in the toast.
pub fn apply(world: &mut World, edits: Vec<PanelEdit>, _dragging: bool) {
    for e in edits {
        match e {
            PanelEdit::Ops(label, merge, ops) => {
                if let Err(err) = super::edit(world, &label, merge, &ops) {
                    world.get_resource_or_insert_with(LogicEditor::default).toast =
                        format!("{label}: {} ({})", err.error.message, err.error.code);
                }
            }
            PanelEdit::Select(node) => {
                world.get_resource_or_insert_with(LogicEditor::default).selected = node
            }
            PanelEdit::Watch(bits) => world.get_resource_or_insert_with(LogicEditor::default).watch = bits,
            PanelEdit::Control(control) => {
                world
                    .get_resource_or_insert_with(LogicEditor::default)
                    .pending_control = Some(control);
            }
            PanelEdit::Save => {
                if let Err(e) = super::save(world, None) {
                    world.get_resource_or_insert_with(LogicEditor::default).toast =
                        format!("not saved: {} ({})", e.message, e.code);
                }
            }
            PanelEdit::Open(path) => {
                let resolved = crate::state_tree_editor::resolve_path(world, &path);
                let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
                if let Err(e) = editor.open(&resolved) {
                    editor.toast = e;
                }
            }
            PanelEdit::New(path) => {
                let resolved =
                    (!path.is_empty()).then(|| crate::state_tree_editor::resolve_path(world, &path));
                world
                    .get_resource_or_insert_with(LogicEditor::default)
                    .new_graph(resolved);
            }
            PanelEdit::Breakpoint(kind, name, change) => {
                // `set_breakpoint` writes its own toast, refusals included.
                let _ = super::set_breakpoint(world, kind, &name, change);
            }
            PanelEdit::GoToNode(node) => {
                if let Err((code, message)) = super::problems::go_to_node(world, node) {
                    world.get_resource_or_insert_with(LogicEditor::default).toast =
                        format!("{message} ({code})");
                }
            }
            PanelEdit::Variable(change) => {
                let toast = match super::variables::apply(world, &change) {
                    Ok(()) => change.label(),
                    Err((code, message)) => format!("{}: {message} ({code})", change.label()),
                };
                world.get_resource_or_insert_with(LogicEditor::default).toast = toast;
            }
            PanelEdit::WatchAdd(item) => {
                let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
                let editor = &mut *editor;
                match editor.watches.add(item) {
                    Ok(()) => editor.watches.recompute(&editor.doc),
                    Err(e) => editor.toast = e,
                }
            }
            PanelEdit::WatchRemove(item) => {
                let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
                let editor = &mut *editor;
                editor.watches.remove(&item);
                editor.watches.recompute(&editor.doc);
            }
            PanelEdit::WatchLive(on) => {
                world
                    .get_resource_or_insert_with(LogicEditor::default)
                    .watches
                    .live = on;
            }
            PanelEdit::Evaluate(text) => {
                let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
                let result = super::watch::evaluate(editor.watches.snapshot.as_ref(), &text);
                editor.watches.evaluation = Some((text, result));
            }
            PanelEdit::FindUsages(query) => {
                super::usages::find(world, &query);
            }
            PanelEdit::OpenUsage(index) => {
                if let Err((code, message)) = super::usages::open_usage(world, index) {
                    world.get_resource_or_insert_with(LogicEditor::default).toast =
                        format!("{message} ({code})");
                }
            }
        }
    }
}

/// F9 on the panel: toggles the breakpoint of the selected node's rule.
fn f9_edit(editor: &LogicEditor, key: Option<&str>) -> Option<PanelEdit> {
    let rule = super::breakpoints::rule_of_node(&editor.doc, editor.selected?)?;
    let has = key.is_some_and(|key| {
        editor
            .breakpoints
            .file
            .graphs
            .get(key)
            .is_some_and(|g| g.get(BreakpointKind::Rule, &rule).is_some())
    });
    let change = if has {
        BreakpointChange::Remove
    } else {
        BreakpointChange::Enable(true)
    };
    Some(PanelEdit::Breakpoint(BreakpointKind::Rule, rule, change))
}

fn draw(ui: &mut egui::Ui, world: &mut World) -> Vec<PanelEdit> {
    let mut edits = Vec::new();
    let tasks = super::palette_tasks(world);
    let revision = world.get_resource::<LogicEditor>().map_or(0, |e| e.revision);
    let cached = world
        .get_resource::<LogicEditor>()
        .and_then(|e| e.ui.issues.clone())
        .filter(|(rev, _)| *rev == revision);
    let issues = match cached {
        Some((_, issues)) => issues,
        None => std::sync::Arc::new(super::diagnose(world)),
    };
    let root = crate::agent_ext::agent_root(world);
    let tokens = crate::theme::tokens(world).clone();
    let usage_answer = world
        .get_resource::<super::usages::LogicUsages>()
        .and_then(|u| u.last.clone());
    let mut editor = world.remove_resource::<LogicEditor>().unwrap_or_default();
    editor.breakpoints.ensure_loaded(&root);
    let key = super::breakpoint_key(&editor, &root);
    let bps = key
        .as_deref()
        .map(|k| editor.breakpoints.of(k))
        .unwrap_or_default();
    let mut state = std::mem::take(&mut editor.ui);
    // Debugger edits once per frame (egui may run this panel twice in one frame).
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = state.debug_frame != Some(frame);
    state.issues = Some((revision, issues.clone()));
    let owners = match &state.owners {
        Some((rev, owners)) if *rev == revision => owners.clone(),
        _ => {
            let owners = std::sync::Arc::new(node_owners(&editor.doc));
            state.owners = Some((revision, owners.clone()));
            owners
        }
    };
    toolbar(ui, &editor, &mut state, &mut edits);
    if editor.is_open()
        && fresh
        && ui.input(|i| i.key_pressed(egui::Key::F9))
        && let Some(edit) = f9_edit(&editor, key.as_deref())
    {
        edits.push(edit);
    }
    if editor.is_open() {
        let errors: BTreeMap<NodeId, String> = issues
            .iter()
            .filter_map(|i| i.node.map(|n| (n, format!("{}: {}", i.code, i.message))))
            .collect();
        egui::Panel::left("logic-palette")
            .resizable(true)
            .default_size(170.0)
            .show(ui, |ui| {
                palette_ui(ui, &editor.doc, &tasks, &mut state, &mut edits)
            });
        egui::Panel::left("logic-nodes")
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| {
                let toast = nodes_ui(ui, &editor.doc, editor.selected, &tasks, &mut state, &mut edits);
                if let Some(toast) = toast {
                    editor.toast = toast;
                }
            });
        egui::Panel::right("logic-inspector")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| inspector_ui(ui, &editor, &tasks, &mut state, &mut edits));
        egui::Panel::bottom("logic-bottom")
            .resizable(true)
            .default_size(150.0)
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    cols[0].horizontal(|ui| {
                        for (tab, label) in BottomTab::ALL {
                            if ui.selectable_label(state.tab == tab, label).clicked() {
                                state.tab = tab;
                            }
                        }
                    });
                    match state.tab {
                        BottomTab::Issues => issues_ui(&mut cols[0], &issues, &mut edits),
                        BottomTab::Breakpoints => {
                            breakpoints_ui(&mut cols[0], &editor, &bps, key.is_some(), &mut edits);
                        }
                        BottomTab::Variables => variables_ui(&mut cols[0], &editor, &mut state, &mut edits),
                        BottomTab::Watch => watch_ui(&mut cols[0], &editor, &mut state, &mut edits),
                        BottomTab::Usages => {
                            super::usages::usages_tab(&mut cols[0], usage_answer.as_ref(), &root, &mut edits);
                        }
                    }
                    live_ui(&mut cols[1], &editor, &mut state, &mut edits);
                });
            });
        if state.snarl_rev != Some(editor.revision) || state.snarl.is_none() {
            state.snarl = Some(build_snarl(&editor.doc));
            state.snarl_rev = Some(editor.revision);
        }
        if let Some(mut snarl) = state.snarl.take() {
            if !editor.pulse.wires.is_empty() {
                ui.ctx().request_repaint();
            }
            // A Problems link or Go to: draw that node alone once under a fresh canvas id, so
            // egui-snarl's first-frame fit centers the view on it (`graph_kit::canvas::show`). The
            // canvas itself has no keyboard route yet (its CANVAS_ALLOW row, FU-5).
            let framed = state.frame.take().and_then(|node| {
                let (sid, _, _) = snarl.nodes_pos_ids().find(|(_, _, id)| **id == node)?;
                state.frame_epoch += 1;
                Some(sid)
            });
            {
                let mut wire_search = std::mem::take(&mut state.wire_search);
                let mut viewer = Viewer {
                    doc: &editor.doc,
                    tasks: &tasks,
                    errors: &errors,
                    live: editor.live.as_ref(),
                    owners: &owners,
                    selected: editor.selected,
                    edits: &mut edits,
                    breakpoints: &bps,
                    snapshot: editor.watches.snapshot.as_ref(),
                    pulse: &editor.pulse,
                    pulse_color: tokens.accents.active,
                    fresh,
                    wire_search: &mut wire_search,
                };
                let style = SnarlStyle {
                    collapsible: Some(false),
                    ..SnarlStyle::new()
                };
                let salt = (
                    "logic-canvas",
                    editor.path.clone().unwrap_or_default(),
                    state.frame_epoch,
                );
                let name = format!("Logic graph canvas, {} nodes", editor.doc.nodes.len());
                let widget = SnarlWidget::new().id_salt(salt).style(style);
                crate::graph_kit::canvas::show(ui, &widget, &mut snarl, &mut viewer, framed, &name);
                state.wire_search = wire_search;
            }
            // Dragged nodes: one coalesced move step per node.
            for (_, pos, id) in snarl.nodes_pos_ids() {
                if let Some(n) = editor.doc.nodes.get(id)
                    && ((n.pos[0] - pos.x).abs() > 0.5 || (n.pos[1] - pos.y).abs() > 0.5)
                {
                    edits.push(PanelEdit::Ops(
                        "move node".into(),
                        Some(format!("move:{id}")),
                        vec![LogicOp::MoveNode {
                            node: *id,
                            at: [pos.x, pos.y],
                        }],
                    ));
                }
            }
            state.snarl = Some(snarl);
        }
    } else {
        ui.weak("Open a .logic.ron (or start a new graph) to edit it here.");
    }
    let debug_edit = edits.iter().any(|e| {
        matches!(
            e,
            PanelEdit::Breakpoint(..)
                | PanelEdit::WatchAdd(_)
                | PanelEdit::WatchRemove(_)
                | PanelEdit::Evaluate(_)
                | PanelEdit::WatchLive(_)
                | PanelEdit::Variable(_)
                | PanelEdit::FindUsages(_)
                | PanelEdit::OpenUsage(_)
        )
    });
    if !fresh {
        // The second pass of a frame: its debugger edits were taken in the first.
        edits.retain(|e| {
            !matches!(
                e,
                PanelEdit::Breakpoint(..)
                    | PanelEdit::WatchAdd(_)
                    | PanelEdit::WatchRemove(_)
                    | PanelEdit::Evaluate(_)
                    | PanelEdit::WatchLive(_)
                    | PanelEdit::Variable(_)
                    | PanelEdit::FindUsages(_)
                    | PanelEdit::OpenUsage(_)
            )
        });
    } else if debug_edit {
        state.debug_frame = Some(frame);
    }
    editor.ui = state;
    world.insert_resource(editor);
    edits
}

/// The Nodes list beside the canvas: the canvas's keyboard route (FU-5, ADR-8628; the pattern of
/// the material graph's list, `canvas_keys::graph_keys_ui`). Up/Down move between nodes, Enter
/// selects one and frames it, Alt+arrow moves it (one undo step per press), Ctrl+Shift+A adds a
/// node its output feeds, Ctrl+Shift+C starts a wire to a node typed by name (Enter completes it,
/// Escape cancels), Delete removes it. Each edit is the op `editor.logic_edit` takes. Returns a
/// toast for a refused gesture (a wire target that names no node).
fn nodes_ui(
    ui: &mut egui::Ui,
    doc: &LogicGraphDoc,
    selected: Option<NodeId>,
    tasks: &[String],
    state: &mut PanelState,
    edits: &mut Vec<PanelEdit>,
) -> Option<String> {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let ids: Vec<NodeId> = doc.nodes.keys().copied().collect();
    let names: Vec<String> = doc
        .nodes
        .iter()
        .map(|(id, n)| node_row_name(*id, &n.kind))
        .collect();
    // The focus follows its node when rows move (a node added or removed before it).
    if let Some(at) = state.keys_node.and_then(|n| ids.iter().position(|m| *m == n)) {
        state.keys.list.retarget(Some(at));
    }
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let kinds = |row: usize, filter: &str| -> Vec<String> {
        ids.get(row)
            .map(|&from| {
                kinds_after(doc, tasks, from, filter)
                    .into_iter()
                    .map(|(label, _)| label)
                    .collect()
            })
            .unwrap_or_default()
    };
    let rows = GraphRows {
        len: ids.len(),
        name: &name,
        selected: selected.and_then(|s| ids.iter().position(|id| *id == s)),
        kinds: &kinds,
        named_outputs: false,
        read_only: false,
        // One wire pin each way: the target node is the whole form.
        input_pins: false,
        deletable: true,
        heading: "Nodes",
        noun: "node",
    };
    let actions = graph_keys_ui(ui, "logic-nodes", &mut state.keys, &rows);
    state.keys_node = state.keys.list.current.and_then(|i| ids.get(i).copied());
    // egui may run the panel twice in one frame: a key press makes its edit once.
    if actions.is_empty() || !crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("logic-nodes-edit")) {
        return None;
    }
    let mut toast = None;
    for action in actions {
        let node = |row: usize| ids.get(row).copied();
        match action {
            GraphAction::Activate(row) => {
                if let Some(id) = node(row) {
                    edits.push(PanelEdit::Select(Some(id)));
                    state.frame = Some(id);
                }
            }
            GraphAction::Delete(row) => {
                if let Some(id) = node(row) {
                    edits.push(PanelEdit::Ops(
                        "delete node".into(),
                        None,
                        vec![LogicOp::RemoveNode { node: id }],
                    ));
                    if selected == Some(id) {
                        edits.push(PanelEdit::Select(None));
                    }
                }
            }
            GraphAction::Nudge(row, delta) => {
                if let Some((id, n)) = node(row).and_then(|id| doc.nodes.get(&id).map(|n| (id, n))) {
                    // No merge key: each key press is its own undo step.
                    edits.push(PanelEdit::Ops(
                        "move node".into(),
                        None,
                        vec![LogicOp::MoveNode {
                            node: id,
                            at: [n.pos[0] + delta[0], n.pos[1] + delta[1]],
                        }],
                    ));
                }
            }
            GraphAction::AddAfter(row, kind) => {
                let Some((from, n)) = node(row).and_then(|id| doc.nodes.get(&id).map(|n| (id, n))) else {
                    continue;
                };
                let Some((label, op)) = kinds_after(doc, tasks, from, "")
                    .into_iter()
                    .find(|(label, _)| *label == kind)
                else {
                    continue;
                };
                let at = [n.pos[0] + regia_ai::logic_edit::COLUMN_WIDTH, n.pos[1]];
                edits.push(PanelEdit::Ops(
                    format!("add {label}"),
                    None,
                    add_wired_ops(doc, &op, at, Some(WireFrom::Output(from))),
                ));
            }
            GraphAction::Connect(row, form) => {
                let Some(from) = node(row) else {
                    continue;
                };
                match find_node(doc, &form.target) {
                    Some(to) => {
                        edits.push(PanelEdit::Ops("connect".into(), None, connect_ops(doc, from, to)))
                    }
                    None => {
                        toast = Some(format!(
                            "connect: no single node is named `{}` (type its row name or #id)",
                            form.target.trim()
                        ));
                    }
                }
            }
        }
    }
    toast
}

fn toolbar(ui: &mut egui::Ui, editor: &LogicEditor, state: &mut PanelState, edits: &mut Vec<PanelEdit>) {
    ui.horizontal(|ui| {
        ui.menu_button("New…", |ui| {
            ui.label("Path (.logic.ron)");
            ui.text_edit_singleline(&mut state.new_path);
            if ui.button("Create").clicked() {
                edits.push(PanelEdit::New(state.new_path.clone()));
                ui.close();
            }
        });
        ui.menu_button("Open…", |ui| {
            ui.label("Path (.logic.ron)");
            ui.text_edit_singleline(&mut state.open_path);
            if ui.button("Open").clicked() && !state.open_path.is_empty() {
                edits.push(PanelEdit::Open(state.open_path.clone()));
                ui.close();
            }
        });
        let path = editor
            .path
            .as_ref()
            .map_or_else(|| "(unsaved graph)".to_string(), |p| p.display().to_string());
        ui.strong(path);
        if editor.dirty() && editor.is_open() {
            ui.weak("● unsaved");
        }
        if editor.is_open() && ui.button("Save").clicked() {
            edits.push(PanelEdit::Save);
        }
        ui.weak(&editor.toast);
    });
}

fn palette_ui(
    ui: &mut egui::Ui,
    doc: &LogicGraphDoc,
    tasks: &[String],
    state: &mut PanelState,
    edits: &mut Vec<PanelEdit>,
) {
    ui.strong("Palette");
    let field = ui.add(
        egui::TextEdit::singleline(&mut state.search)
            .id_salt("logic-palette-search")
            .hint_text("Search"),
    );
    let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Search nodes");
    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let column_of = |group: &str| match group {
        "Events" => 0.0,
        "Conditions" => 1.0,
        _ => 2.0,
    };
    let place = |group: &str| {
        [
            column_of(group) * regia_ai::logic_edit::COLUMN_WIDTH,
            doc.rules.len() as f32 * regia_ai::logic_edit::ROW_HEIGHT,
        ]
    };
    let entries = if state.search.trim().is_empty() {
        palette(tasks)
    } else {
        palette_search(doc, tasks, &state.search, None)
    };
    // Enter in the search box adds the best match.
    if enter
        && !state.search.trim().is_empty()
        && let Some((group, label, op)) = entries.first()
    {
        edits.push(PanelEdit::Ops(
            format!("add {label}"),
            None,
            vec![placed(doc, op, place(group))],
        ));
        state.search.clear();
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut group_shown = "";
        for (group, label, op) in entries {
            if group != group_shown {
                ui.separator();
                ui.label(group);
                group_shown = group;
            }
            if ui.button(&label).clicked() {
                let column = match group {
                    "Events" => 0.0,
                    "Conditions" => 1.0,
                    _ => 2.0,
                };
                let at = [
                    column * regia_ai::logic_edit::COLUMN_WIDTH,
                    doc.rules.len() as f32 * regia_ai::logic_edit::ROW_HEIGHT,
                ];
                edits.push(PanelEdit::Ops(
                    format!("add {label}"),
                    None,
                    vec![placed(doc, &op, at)],
                ));
            }
        }
    });
}

/// A RON text field: edits a draft and returns the parsed value when Enter is pressed.
fn ron_field<T: serde::Serialize + serde::de::DeserializeOwned>(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    node: NodeId,
    field: &str,
    value: &T,
) -> Option<T> {
    let key = (node, field.to_string());
    let draft = state
        .drafts
        .entry(key.clone())
        .or_insert_with(|| ron::to_string(value).unwrap_or_default());
    let response = ui.text_edit_singleline(draft);
    let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &format!("{field} (RON)"));
    let parsed = ron::from_str::<T>(draft);
    if parsed.is_err() {
        ui.colored_label(egui::Color32::from_rgb(230, 80, 70), "not valid RON");
    }
    if response.lost_focus() {
        let out = parsed.ok();
        state.drafts.remove(&key);
        return out;
    }
    None
}

fn inspector_ui(
    ui: &mut egui::Ui,
    editor: &LogicEditor,
    tasks: &[String],
    state: &mut PanelState,
    edits: &mut Vec<PanelEdit>,
) {
    ui.strong("Inspector");
    let Some(id) = editor.selected else {
        ui.weak("Select a node.");
        return;
    };
    let Some(node) = editor.doc.nodes.get(&id) else {
        return;
    };
    // Find usages of what the node names (the keyboard path of its context menu, w13 P-183).
    super::usages::inspector_row(ui, &editor.doc, id, edits);
    let ops = |label: &str, op: LogicOp| {
        PanelEdit::Ops(label.to_string(), Some(format!("inspect:{id}:{label}")), vec![op])
    };
    match &node.kind {
        LogicNodeKind::Event {
            rule,
            event,
            priority,
            cooldown_ticks,
        } => {
            ui.label("Rule name");
            let mut name = rule.clone();
            let field = ui.text_edit_singleline(&mut name);
            if crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Rule name").changed() {
                edits.push(ops(
                    "rename rule",
                    LogicOp::SetEvent {
                        node: id,
                        rule: Some(name),
                        event: None,
                        priority: None,
                        cooldown_ticks: None,
                    },
                ));
            }
            ui.label("Event (RON)");
            if let Some(event) = ron_field(ui, state, id, "event", event) {
                edits.push(ops(
                    "set event",
                    LogicOp::SetEvent {
                        node: id,
                        rule: None,
                        event: Some(event),
                        priority: None,
                        cooldown_ticks: None,
                    },
                ));
            }
            let mut p = *priority;
            let field = ui.add(egui::DragValue::new(&mut p).prefix("priority "));
            if crate::a11y_audit::named_drag_value(field, "Priority", f64::from(p)).changed() {
                edits.push(ops(
                    "set priority",
                    LogicOp::SetEvent {
                        node: id,
                        rule: None,
                        event: None,
                        priority: Some(p),
                        cooldown_ticks: None,
                    },
                ));
            }
            let mut c = *cooldown_ticks;
            let field = ui.add(egui::DragValue::new(&mut c).prefix("cooldown ticks "));
            if crate::a11y_audit::named_drag_value(field, "Cooldown ticks", f64::from(c)).changed() {
                edits.push(ops(
                    "set cooldown",
                    LogicOp::SetEvent {
                        node: id,
                        rule: None,
                        event: None,
                        priority: None,
                        cooldown_ticks: Some(c),
                    },
                ));
            }
        }
        LogicNodeKind::Condition { condition } => {
            ui.horizontal(|ui| {
                let current = CondMode::of(condition);
                for (mode, label) in [
                    (CondMode::Leaf, "leaf"),
                    (CondMode::All, "all"),
                    (CondMode::Any, "any"),
                    (CondMode::Not, "not"),
                ] {
                    if ui.selectable_label(current == mode, label).clicked() && current != mode {
                        edits.push(PanelEdit::Ops(
                            format!("condition mode {label}"),
                            None,
                            vec![LogicOp::SetConditionMode { node: id, mode }],
                        ));
                    }
                }
            });
            ui.label("Condition (RON)");
            if let Some(c) = ron_field(ui, state, id, "condition", condition) {
                edits.push(PanelEdit::Ops(
                    "set condition".into(),
                    None,
                    vec![LogicOp::SetCondition {
                        node: id,
                        condition: c,
                    }],
                ));
            }
        }
        LogicNodeKind::Action { task, set } => {
            let combo = egui::ComboBox::from_id_salt(("logic-task", id))
                .selected_text(task.as_str())
                .show_ui(ui, |ui| {
                    for t in tasks {
                        if ui.selectable_label(t == task, t).clicked() && t != task {
                            edits.push(PanelEdit::Ops(
                                "set task".into(),
                                None,
                                vec![LogicOp::SetTask {
                                    node: id,
                                    task: t.clone(),
                                }],
                            ));
                        }
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Task");
            ui.label("Parameters (RON bindings)");
            for (key, binding) in set {
                ui.horizontal(|ui| {
                    ui.label(key);
                    if let Some(b) = ron_field::<Binding>(ui, state, id, &format!("set:{key}"), binding) {
                        edits.push(PanelEdit::Ops(
                            "set parameter".into(),
                            None,
                            vec![LogicOp::SetBinding {
                                node: id,
                                key: key.clone(),
                                value: Some(b),
                            }],
                        ));
                    }
                    if ui.small_button("\u{1f5d9}").clicked() {
                        edits.push(PanelEdit::Ops(
                            "remove parameter".into(),
                            None,
                            vec![LogicOp::SetBinding {
                                node: id,
                                key: key.clone(),
                                value: None,
                            }],
                        ));
                    }
                });
            }
            ui.horizontal(|ui| {
                let field = ui.text_edit_singleline(&mut state.new_key);
                crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "New parameter name");
                if ui.button("Add parameter").clicked() && !state.new_key.is_empty() {
                    edits.push(PanelEdit::Ops(
                        "add parameter".into(),
                        None,
                        vec![LogicOp::SetBinding {
                            node: id,
                            key: std::mem::take(&mut state.new_key),
                            value: Some(Binding::Literal(Literal::String(String::new()))),
                        }],
                    ));
                }
            });
        }
    }
    if ui.button("Delete node").clicked() {
        edits.push(PanelEdit::Ops(
            "delete node".into(),
            None,
            vec![LogicOp::RemoveNode { node: id }],
        ));
    }
}

fn issues_ui(ui: &mut egui::Ui, issues: &[LogicIssue], edits: &mut Vec<PanelEdit>) {
    ui.strong(format!("Issues ({})", issues.len()));
    egui::ScrollArea::vertical()
        .id_salt("logic-issues")
        .show(ui, |ui| {
            for (code, severity, node, message) in issue_rows(issues) {
                let color = if severity == "error" {
                    egui::Color32::from_rgb(230, 80, 70)
                } else {
                    egui::Color32::from_rgb(240, 180, 60)
                };
                let text = match node {
                    Some(n) => format!("{code} #{n}: {message}"),
                    None => format!("{code}: {message}"),
                };
                let row =
                    egui::Label::new(egui::RichText::new(text).color(color)).sense(egui::Sense::click());
                if ui.add(row).clicked()
                    && let Some(n) = node
                {
                    edits.push(PanelEdit::Select(Some(n)));
                }
            }
        });
}

fn live_ui(ui: &mut egui::Ui, editor: &LogicEditor, state: &mut PanelState, edits: &mut Vec<PanelEdit>) {
    ui.strong("Live");
    if editor.agents.is_empty() {
        ui.weak("Play the game to see running graphs.");
        return;
    }
    let combo = egui::ComboBox::from_id_salt("logic-watch")
        .selected_text(
            editor
                .live
                .as_ref()
                .map_or_else(|| "—".into(), |l| format!("entity {}", l.entity)),
        )
        .show_ui(ui, |ui| {
            for bits in &editor.agents {
                if ui
                    .selectable_label(editor.watch == Some(*bits), format!("entity {bits}"))
                    .clicked()
                {
                    edits.push(PanelEdit::Watch(Some(*bits)));
                }
            }
        });
    crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Watched graph");
    if let Some(reload) = &editor.reload {
        ui.weak(format!(
            "save {}: {} queued, {} kept, {} restarted, {} rejected",
            reload.save, reload.queued, reload.kept, reload.restarted, reload.rejected
        ));
    }
    let Some(live) = &editor.live else {
        return;
    };
    ui.horizontal(|ui| {
        use super::LiveControl;
        if live.running {
            if ui.button("Pause").clicked() {
                edits.push(PanelEdit::Control(LiveControl::Pause));
            }
        } else if ui.button("Resume").clicked() {
            edits.push(PanelEdit::Control(LiveControl::Resume));
        }
        if ui
            .button("Step")
            .on_hover_text("Run one tick, then pause")
            .clicked()
        {
            edits.push(PanelEdit::Control(LiveControl::Step));
        }
        // The runtime stops between ticks only (ADR-8021): the finest step past one tick runs to
        // the next tick in which a rule fires.
        if ui
            .button("Step to next fired rule")
            .on_hover_text("Run until a rule fires, then pause at the end of that tick")
            .clicked()
        {
            edits.push(PanelEdit::Control(LiveControl::StepToFire));
        }
    });
    if !editor.breakpoints.hits.is_empty() {
        ui.colored_label(
            crate::theme::ui_tokens(ui.ctx()).accents.alert,
            format!("Paused at {}", editor.breakpoints.hits.join(", ")),
        );
    }
    evaluate_ui(ui, editor, state, edits);
    ui.label(format!(
        "{} · tick {}{}",
        if live.running { "running" } else { "paused" },
        live.ticks,
        if live.matches_document {
            ""
        } else {
            " · differs from the document"
        }
    ));
    egui::ScrollArea::vertical().id_salt("logic-live").show(ui, |ui| {
        for (rule, activity) in &live.activity {
            ui.colored_label(activity_color(*activity), format!("{rule}: {}", activity.name()));
        }
        egui::Grid::new("logic-vars").striped(true).show(ui, |ui| {
            for (scope, vars) in [
                ("local/", &live.locals),
                ("entity", &live.entity_vars),
                ("world/", &live.world_vars),
            ] {
                for (k, v) in vars {
                    ui.weak(scope);
                    ui.label(k);
                    ui.label(v);
                    ui.end_row();
                }
            }
        });
    });
}

/// The evaluate box (while the watched graph is paused): a condition in the graph's syntax (RON,
/// as the inspector's Condition field takes it) over the paused instance's variables.
fn evaluate_ui(ui: &mut egui::Ui, editor: &LogicEditor, state: &mut PanelState, edits: &mut Vec<PanelEdit>) {
    let paused = editor.watches.snapshot.as_ref().is_some_and(|s| s.paused);
    if !paused {
        return;
    }
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut state.eval_draft)
                .id_salt("logic-evaluate")
                .hint_text("Compare(key: \"local/n\", op: Gt, value: 0.0)"),
        );
        let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Evaluate expression");
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Evaluate").clicked() || enter) && !state.eval_draft.trim().is_empty() {
            edits.push(PanelEdit::Evaluate(state.eval_draft.clone()));
        }
    });
    if let Some((text, result)) = &editor.watches.evaluation {
        let tokens = crate::theme::ui_tokens(ui.ctx());
        match result {
            Ok(value) => ui.label(format!("{text} = {value}")),
            Err(e) => ui.colored_label(tokens.error, e.as_str()),
        };
    }
}

/// The Breakpoints list: each rule and event breakpoint of the open graph with Enable, Go to and
/// Remove (every control a focusable widget).
fn breakpoints_ui(
    ui: &mut egui::Ui,
    editor: &LogicEditor,
    bps: &super::breakpoints::GraphBreakpoints,
    has_path: bool,
    edits: &mut Vec<PanelEdit>,
) {
    if !has_path {
        ui.weak("Save the graph to set breakpoints (they belong to its file).");
        return;
    }
    if let Some(error) = &editor.breakpoints.error {
        ui.colored_label(crate::theme::ui_tokens(ui.ctx()).warn, error);
    }
    if !cfg!(debug_assertions) {
        ui.weak("Breakpoints apply in development builds only.");
    }
    if bps.rules.is_empty() && bps.events.is_empty() {
        ui.weak("No breakpoints: click a rule's dot, press F9 on a selected node, or use an event's menu.");
        return;
    }
    egui::ScrollArea::vertical().id_salt("logic-breakpoints").show(ui, |ui| {
        for kind in [BreakpointKind::Rule, BreakpointKind::Event] {
            for bp in bps.list(kind) {
                ui.horizontal(|ui| {
                    let mut on = bp.enabled;
                    let spoken = format!("Enable the breakpoint on {} {}", kind.name(), bp.name);
                    if crate::a11y_audit::bare_checkbox(ui, &mut on, &spoken).changed() {
                        edits.push(PanelEdit::Breakpoint(kind, bp.name.clone(), BreakpointChange::Enable(on)));
                    }
                    ui.label(format!("{} {}", kind.name(), bp.name));
                    let target = match kind {
                        BreakpointKind::Rule => editor.doc.find_rule(&bp.name),
                        BreakpointKind::Event => editor.doc.rules.iter().copied().find(|id| {
                            matches!(
                                editor.doc.nodes.get(id).map(|n| &n.kind),
                                Some(LogicNodeKind::Event { event, .. })
                                    if super::breakpoints::event_breakpoint_name(event).as_deref() == Some(bp.name.as_str())
                            )
                        }),
                    };
                    if let Some(node) = target
                        && ui
                            .button("Go to")
                            .on_hover_text(format!("Select and frame node #{node}"))
                            .clicked()
                    {
                        edits.push(PanelEdit::GoToNode(node));
                    }
                    let spoken = format!("Remove the breakpoint on {} {}", kind.name(), bp.name);
                    if crate::a11y_audit::glyph_button(ui, "✕", &spoken).clicked() {
                        edits.push(PanelEdit::Breakpoint(kind, bp.name.clone(), BreakpointChange::Remove));
                    }
                });
            }
        }
    });
}

/// A draft text field of the Variables list: returns the text when Enter is pressed in it.
fn var_field(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    key: &str,
    initial: &str,
    spoken: &str,
) -> Option<String> {
    let draft = state
        .var_drafts
        .entry(key.to_string())
        .or_insert_with(|| initial.to_string());
    let field = ui.add(
        egui::TextEdit::singleline(draft)
            .id_salt(("logic-var", key))
            .desired_width(90.0),
    );
    let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, spoken);
    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        return state.var_drafts.remove(key);
    }
    None
}

/// A literal as the Variables list edits it (the text a default field shows).
fn literal_text(value: &Literal) -> String {
    match value {
        Literal::Bool(b) => b.to_string(),
        Literal::Int(i) => i.to_string(),
        Literal::Float(f) => f.to_string(),
        Literal::String(s) => s.clone(),
    }
}

/// The Variables list: locals (rename, type, default, remove), world boards (rename, remove), typed
/// world variables `board/key` and entity variables (v2: rename, type, default, remove) and the
/// untyped entity keys the graph uses (rename), then fields to declare a local, a board, an entity
/// variable or a world variable. Each change is one undo step.
fn variables_ui(ui: &mut egui::Ui, editor: &LogicEditor, state: &mut PanelState, edits: &mut Vec<PanelEdit>) {
    egui::ScrollArea::vertical()
        .id_salt("logic-variables")
        .show(ui, |ui| {
            egui::Grid::new("logic-variables-grid")
                .striped(true)
                .show(ui, |ui| {
                    for row in super::variables::rows(&editor.doc) {
                        let scope = row.scope;
                        ui.weak(scope.name());
                        let key = format!("{}:{}", scope.name(), row.name);
                        if let Some(to) = var_field(
                            ui,
                            state,
                            &format!("name:{key}"),
                            &row.name,
                            &format!("Rename {} variable {}", scope.name(), row.name),
                        ) && to != row.name
                        {
                            edits.push(PanelEdit::Variable(VariableEdit::Rename {
                                scope,
                                from: row.name.clone(),
                                to,
                            }));
                        }
                        match &row.default {
                            Some(default) => {
                                let current = VarType::of(default);
                                let combo = egui::ComboBox::from_id_salt(("logic-var-type", &key))
                                    .selected_text(current.name())
                                    .show_ui(ui, |ui| {
                                        for ty in VarType::ALL {
                                            if ui.selectable_label(ty == current, ty.name()).clicked()
                                                && ty != current
                                            {
                                                edits.push(PanelEdit::Variable(VariableEdit::Set {
                                                    scope,
                                                    name: row.name.clone(),
                                                    ty: Some(ty),
                                                    default: None,
                                                }));
                                            }
                                        }
                                    });
                                crate::a11y_audit::name(
                                    combo.response,
                                    egui::WidgetType::ComboBox,
                                    &format!("Type of {}", row.name),
                                );
                                if let Some(text) = var_field(
                                    ui,
                                    state,
                                    &format!("default:{key}"),
                                    &literal_text(default),
                                    &format!("Default of {}", row.name),
                                ) {
                                    let value = current.convert(&Literal::String(text));
                                    edits.push(PanelEdit::Variable(VariableEdit::Set {
                                        scope,
                                        name: row.name.clone(),
                                        ty: Some(current),
                                        default: Some(value),
                                    }));
                                }
                            }
                            None => {
                                ui.weak(match scope {
                                    VarScope::World => "board",
                                    _ => "entity key",
                                });
                                ui.label("");
                            }
                        }
                        ui.weak(format!("{} uses", row.uses));
                        // A used entity key has nothing to remove; a declared one (v2) does.
                        if (scope != VarScope::Entity || row.default.is_some())
                            && crate::a11y_audit::glyph_button(
                                ui,
                                "✕",
                                &format!("Remove {} variable {}", scope.name(), row.name),
                            )
                            .clicked()
                        {
                            edits.push(PanelEdit::Variable(VariableEdit::Remove {
                                scope,
                                name: row.name.clone(),
                            }));
                        }
                        ui.end_row();
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                let ty = state.new_local_type.unwrap_or(VarType::Int);
                if let Some(name) = var_field(ui, state, "new_local", "", "New local name")
                    && !name.is_empty()
                {
                    edits.push(PanelEdit::Variable(VariableEdit::Set {
                        scope: VarScope::Local,
                        name,
                        ty: Some(ty),
                        default: None,
                    }));
                }
                let combo = egui::ComboBox::from_id_salt("logic-new-local-type")
                    .selected_text(ty.name())
                    .show_ui(ui, |ui| {
                        for t in VarType::ALL {
                            if ui.selectable_label(t == ty, t.name()).clicked() {
                                state.new_local_type = Some(t);
                            }
                        }
                    });
                crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "New local type");
                ui.weak("local");
            });
            ui.horizontal(|ui| {
                if let Some(name) = var_field(ui, state, "new_board", "", "New world board name")
                    && !name.is_empty()
                {
                    edits.push(PanelEdit::Variable(VariableEdit::Set {
                        scope: VarScope::World,
                        name,
                        ty: None,
                        default: None,
                    }));
                }
                ui.weak("world board");
            });
            // `.logic.ron` v2 (ADR-8900): typed entity and world variables, with the type the
            // local type menu shows.
            ui.horizontal(|ui| {
                let ty = state.new_local_type.unwrap_or(VarType::Int);
                if let Some(name) = var_field(ui, state, "new_entity_var", "", "New entity variable name")
                    && !name.is_empty()
                {
                    edits.push(PanelEdit::Variable(VariableEdit::Set {
                        scope: VarScope::Entity,
                        name,
                        ty: Some(ty),
                        default: None,
                    }));
                }
                ui.weak(format!("entity variable ({})", ty.name()));
            });
            ui.horizontal(|ui| {
                let ty = state.new_local_type.unwrap_or(VarType::Int);
                if let Some(name) = var_field(ui, state, "new_world_var", "", "New world variable, board/key")
                    && !name.is_empty()
                {
                    edits.push(PanelEdit::Variable(VariableEdit::Set {
                        scope: VarScope::World,
                        name,
                        ty: Some(ty),
                        default: None,
                    }));
                }
                ui.weak(format!("world variable board/key ({})", ty.name()));
            });
        });
}

/// The Watch list: pinned variables and condition results with their values at the snapshot, the
/// Live toggle and a field to pin a key.
fn watch_ui(ui: &mut egui::Ui, editor: &LogicEditor, state: &mut PanelState, edits: &mut Vec<PanelEdit>) {
    let watches = &editor.watches;
    ui.horizontal(|ui| {
        let mut live = watches.live;
        if ui
            .checkbox(&mut live, "Live")
            .on_hover_text("Refresh every frame while playing (off: only when the graph pauses)")
            .changed()
        {
            edits.push(PanelEdit::WatchLive(live));
        }
        match &watches.snapshot {
            Some(s) => ui.weak(format!(
                "tick {}{}",
                s.ticks,
                if s.paused { ", paused" } else { "" }
            )),
            None => ui.weak("no values yet"),
        };
    });
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut state.watch_draft)
                .id_salt("logic-watch-add")
                .hint_text("local/n"),
        );
        let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Watch a variable");
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Watch").clicked() || enter) && !state.watch_draft.trim().is_empty() {
            edits.push(PanelEdit::WatchAdd(WatchItem::Key(
                state.watch_draft.trim().to_string(),
            )));
            state.watch_draft.clear();
        }
    });
    let tokens = crate::theme::ui_tokens(ui.ctx());
    egui::Grid::new("logic-watch-grid").striped(true).show(ui, |ui| {
        for (i, item) in watches.items.iter().enumerate() {
            let label = item.label(&editor.doc);
            ui.label(&label);
            match watches.values.get(i) {
                Some(Ok(v)) => ui.label(v),
                Some(Err(e)) => ui.colored_label(tokens.warn, e),
                None => ui.weak("—"),
            };
            if crate::a11y_audit::glyph_button(ui, "✕", &format!("Stop watching {label}")).clicked() {
                edits.push(PanelEdit::WatchRemove(item.clone()));
            }
            ui.end_row();
        }
    });
}
