// SPDX-License-Identifier: MPL-2.0
//! Headless editing of logic graphs (`docs/specs/102-logic-graph.md` § 6, phase 2): the node
//! document the Logic Graph panel and the `editor.logic_*` agent methods share ([`LogicGraphDoc`]),
//! the structural operations on it ([`LogicOp`], mirroring [`crate::bt_edit::BtOp`]), the validator
//! with stable issue codes ([`validate`]) and the lowering to the `.logic.ron` document
//! ([`LogicGraphDoc::to_def`]).
//!
//! # The node model
//!
//! A rule is one **event** node wired through zero or more **condition** nodes into zero or more
//! **action** nodes, in execution order. Every node has at most one wire out and one wire in, so
//! each rule is a chain. Pin types: an event has only an output; a condition's output goes to a
//! condition or an action; an action's output goes to the next action (never back to a condition).
//!
//! Lowering: no condition node is `Always`; one node gives its condition unchanged; a chain of two
//! or more gives `All` of them in wire order. The node's own condition is any `Condition` tree; its
//! *mode* (`all`, `any`, `not`, or a leaf) is the tree's top level and [`LogicOp::SetConditionMode`]
//! re-wraps the same terms. Raising a definition into nodes is the inverse: `Always` → no node,
//! `All` with two or more terms → one node per term, anything else → one node; so
//! `def → doc → def` is exact and re-saving an unchanged file writes the same bytes.
//!
//! Positions are editor-only: they live in the `.logic.layout.ron` sidecar ([`LogicLayoutFile`]),
//! keyed by rule name and role ([`LogicGraphDoc::layout`]), never in the frozen `.logic.ron`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::BtTaskRegistry;
use crate::logic::{
    ActionDef, Binding, EntityVarDef, LOGIC_FORMAT_VERSION, LocalDef, LogicEvent, LogicGraph, LogicGraphDef,
    LogicGraphError, MAX_RULES, RuleDef, WorldVarDef,
};
use crate::state_tree::{Condition, Literal};

/// A node id inside one [`LogicGraphDoc`] (stable while the document is open).
pub type NodeId = u32;

/// Current version of the `.logic.layout.ron` sidecar.
pub const LOGIC_LAYOUT_VERSION: u32 = 1;

/// Horizontal spacing of the default layout (graph units).
pub const COLUMN_WIDTH: f32 = 240.0;

/// Vertical spacing of the default layout (graph units).
pub const ROW_HEIGHT: f32 = 160.0;

/// What a node is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogicNodeKind {
    /// The rule head: its name, event arm, priority and cooldown.
    Event {
        /// The rule's name.
        rule: String,
        /// The event arm.
        event: LogicEvent,
        /// Priority among rules on the same event.
        #[serde(default)]
        priority: i32,
        /// Cooldown in fixed steps.
        #[serde(default)]
        cooldown_ticks: u32,
    },
    /// A guard; its condition joins the rule's condition with `All`.
    Condition {
        /// The node's condition tree.
        condition: Condition,
    },
    /// One action of the chain.
    Action {
        /// The task path.
        task: String,
        /// The parameters.
        #[serde(default)]
        set: BTreeMap<String, Binding>,
    },
}

impl LogicNodeKind {
    /// `event`, `condition` or `action`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Event { .. } => "event",
            Self::Condition { .. } => "condition",
            Self::Action { .. } => "action",
        }
    }
}

/// One node: what it is and where the canvas draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicNode {
    /// What the node is.
    #[serde(flatten)]
    pub kind: LogicNodeKind,
    /// Canvas position (graph units).
    pub pos: [f32; 2],
}

/// The editor's logic graph document: the graph-level fields of [`LogicGraphDef`] plus the rule
/// nodes, their wires and positions. Pure data: no egui, so the agent edits the same document the
/// panel draws.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicGraphDoc {
    /// What the graph is for.
    pub description: String,
    /// The timer-jitter seed.
    pub seed: u64,
    /// Graph locals.
    pub locals: Vec<LocalDef>,
    /// Declared world blackboards.
    pub world: Vec<String>,
    /// Typed entity variables (`.logic.ron` v2, ADR-8900).
    #[serde(default)]
    pub entity_vars: Vec<EntityVarDef>,
    /// Typed world variables (`.logic.ron` v2, ADR-8900).
    #[serde(default)]
    pub world_vars: Vec<WorldVarDef>,
    /// Every node by id.
    pub nodes: BTreeMap<NodeId, LogicNode>,
    /// Wires by source node: `from → to` (one wire out of a node, one into a node).
    pub wires: BTreeMap<NodeId, NodeId>,
    /// The event nodes in rule declaration order.
    pub rules: Vec<NodeId>,
    /// The id the next added node gets.
    pub next_id: NodeId,
}

impl Default for LogicGraphDoc {
    fn default() -> Self {
        Self::new()
    }
}

/// How a condition node composes its terms (the top level of its tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CondMode {
    /// Every term holds.
    All,
    /// At least one term holds.
    Any,
    /// The terms (joined with `All` when there are several) do not hold.
    Not,
    /// A single leaf test (or `Always`).
    Leaf,
}

impl CondMode {
    /// The mode of a condition tree.
    #[must_use]
    pub fn of(condition: &Condition) -> Self {
        match condition {
            Condition::All(_) => Self::All,
            Condition::Any(_) => Self::Any,
            Condition::Not(_) => Self::Not,
            _ => Self::Leaf,
        }
    }
}

/// Re-wraps a condition's terms under `mode` (`Leaf` keeps a single term, or the first of several).
#[must_use]
pub fn with_mode(condition: &Condition, mode: CondMode) -> Condition {
    let terms: Vec<Condition> = match condition {
        Condition::All(v) | Condition::Any(v) => v.clone(),
        Condition::Not(inner) => vec![(**inner).clone()],
        Condition::Always => Vec::new(),
        leaf => vec![leaf.clone()],
    };
    match mode {
        CondMode::All => Condition::All(terms),
        CondMode::Any => Condition::Any(terms),
        CondMode::Not => {
            let inner = if terms.len() == 1 {
                terms.into_iter().next().unwrap_or_default()
            } else {
                Condition::All(terms)
            };
            Condition::Not(Box::new(inner))
        }
        CondMode::Leaf => terms.into_iter().next().unwrap_or_default(),
    }
}

/// The layout sidecar key of the event node of `rule`.
#[must_use]
pub fn event_key(rule: &str) -> String {
    format!("{rule}#event")
}

/// The layout sidecar key of the `index`-th condition node of `rule`.
#[must_use]
pub fn condition_key(rule: &str, index: usize) -> String {
    format!("{rule}#condition{index}")
}

/// The layout sidecar key of the `index`-th action node of `rule`.
#[must_use]
pub fn action_key(rule: &str, index: usize) -> String {
    format!("{rule}#action{index}")
}

/// One rule as wired in a document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleChain {
    /// The event node.
    pub event: NodeId,
    /// The condition nodes, in wire order.
    pub conditions: Vec<NodeId>,
    /// The action nodes, in wire order.
    pub actions: Vec<NodeId>,
}

impl LogicGraphDoc {
    /// An empty document at the current version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            description: String::new(),
            seed: 0,
            locals: Vec::new(),
            world: Vec::new(),
            entity_vars: Vec::new(),
            world_vars: Vec::new(),
            nodes: BTreeMap::new(),
            wires: BTreeMap::new(),
            rules: Vec::new(),
            next_id: 1,
        }
    }

    /// Raises a definition into nodes (ids from 1 in rule order: event, conditions, actions) placed
    /// at `positions` (layout keys, [`LogicGraphDoc::layout`]) or, for a key without one, in the
    /// default three-column layout.
    #[must_use]
    pub fn from_def(def: &LogicGraphDef, positions: &BTreeMap<String, [f32; 2]>) -> Self {
        let mut doc = Self {
            description: def.description.clone(),
            seed: def.seed,
            locals: def.locals.clone(),
            world: def.world.clone(),
            entity_vars: def.entity_vars.clone(),
            world_vars: def.world_vars.clone(),
            ..Self::new()
        };
        for (ri, rule) in def.rules.iter().enumerate() {
            let row = ri as f32 * ROW_HEIGHT;
            let place = |key: String, column: f32| {
                positions
                    .get(&key)
                    .copied()
                    .filter(|p| p.iter().all(|v| v.is_finite()))
                    .unwrap_or([column * COLUMN_WIDTH, row])
            };
            let event = doc.insert(LogicNode {
                kind: LogicNodeKind::Event {
                    rule: rule.name.clone(),
                    event: rule.event.clone(),
                    priority: rule.priority,
                    cooldown_ticks: rule.cooldown_ticks,
                },
                pos: place(event_key(&rule.name), 0.0),
            });
            doc.rules.push(event);
            let terms: Vec<Condition> = match &rule.condition {
                Condition::Always => Vec::new(),
                Condition::All(terms) if terms.len() >= 2 => terms.clone(),
                other => vec![other.clone()],
            };
            let mut last = event;
            let n_terms = terms.len();
            for (ci, condition) in terms.into_iter().enumerate() {
                let id = doc.insert(LogicNode {
                    kind: LogicNodeKind::Condition { condition },
                    pos: place(condition_key(&rule.name, ci), 1.0 + ci as f32),
                });
                doc.wires.insert(last, id);
                last = id;
            }
            let first_action_column = 1.0 + n_terms.max(1) as f32;
            for (ai, action) in rule.actions.iter().enumerate() {
                let id = doc.insert(LogicNode {
                    kind: LogicNodeKind::Action {
                        task: action.task.clone(),
                        set: action.set.clone(),
                    },
                    pos: place(action_key(&rule.name, ai), first_action_column + ai as f32),
                });
                doc.wires.insert(last, id);
                last = id;
            }
        }
        doc
    }

    fn insert(&mut self, node: LogicNode) -> NodeId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.nodes.insert(id, node);
        id
    }

    /// The node wired into `node`, if any.
    #[must_use]
    pub fn wire_into(&self, node: NodeId) -> Option<NodeId> {
        self.wires
            .iter()
            .find(|(_, to)| **to == node)
            .map(|(from, _)| *from)
    }

    /// The rule's name of an event node.
    #[must_use]
    pub fn rule_name(&self, event: NodeId) -> Option<&str> {
        match &self.nodes.get(&event)?.kind {
            LogicNodeKind::Event { rule, .. } => Some(rule),
            _ => None,
        }
    }

    /// The event node of the rule named `name` (the first, if two share it).
    #[must_use]
    pub fn find_rule(&self, name: &str) -> Option<NodeId> {
        self.rules
            .iter()
            .copied()
            .find(|id| self.rule_name(*id) == Some(name))
    }

    /// Follows the wires out of an event node: its conditions, then its actions.
    #[must_use]
    pub fn chain(&self, event: NodeId) -> RuleChain {
        let mut chain = RuleChain {
            event,
            ..RuleChain::default()
        };
        let mut seen = BTreeSet::from([event]);
        let mut at = event;
        while let Some(&next) = self.wires.get(&at) {
            if !seen.insert(next) {
                break;
            }
            match self.nodes.get(&next).map(|n| &n.kind) {
                Some(LogicNodeKind::Condition { .. }) if chain.actions.is_empty() => {
                    chain.conditions.push(next)
                }
                Some(LogicNodeKind::Action { .. }) => chain.actions.push(next),
                _ => break,
            }
            at = next;
        }
        chain
    }

    /// Every rule's chain, in declaration order.
    #[must_use]
    pub fn chains(&self) -> Vec<RuleChain> {
        self.rules.iter().map(|e| self.chain(*e)).collect()
    }

    /// Condition and action nodes no event reaches (`dangling_wire`), in id order.
    #[must_use]
    pub fn dangling(&self) -> Vec<NodeId> {
        let reached: BTreeSet<NodeId> = self
            .chains()
            .into_iter()
            .flat_map(|c| std::iter::once(c.event).chain(c.conditions).chain(c.actions))
            .collect();
        self.nodes
            .keys()
            .copied()
            .filter(|id| !reached.contains(id))
            .collect()
    }

    /// Lowers the document to a `.logic.ron` definition. Nodes no event reaches are left out (the
    /// validator reports them as `dangling_wire`).
    #[must_use]
    pub fn to_def(&self) -> LogicGraphDef {
        let mut rules = Vec::with_capacity(self.rules.len());
        for chain in self.chains() {
            let Some(LogicNodeKind::Event {
                rule,
                event,
                priority,
                cooldown_ticks,
            }) = self.nodes.get(&chain.event).map(|n| &n.kind)
            else {
                continue;
            };
            let mut terms: Vec<Condition> = chain
                .conditions
                .iter()
                .filter_map(|id| match &self.nodes.get(id)?.kind {
                    LogicNodeKind::Condition { condition } => Some(condition.clone()),
                    _ => None,
                })
                .collect();
            let condition = match terms.len() {
                0 => Condition::Always,
                1 => terms.pop().unwrap_or_default(),
                _ => Condition::All(terms),
            };
            let actions = chain
                .actions
                .iter()
                .filter_map(|id| match &self.nodes.get(id)?.kind {
                    LogicNodeKind::Action { task, set } => Some(ActionDef {
                        task: task.clone(),
                        set: set.clone(),
                    }),
                    _ => None,
                })
                .collect();
            rules.push(RuleDef {
                name: rule.clone(),
                event: event.clone(),
                priority: *priority,
                cooldown_ticks: *cooldown_ticks,
                condition,
                actions,
            });
        }
        LogicGraphDef {
            format_version: LOGIC_FORMAT_VERSION,
            description: self.description.clone(),
            seed: self.seed,
            locals: self.locals.clone(),
            world: self.world.clone(),
            entity_vars: self.entity_vars.clone(),
            world_vars: self.world_vars.clone(),
            rules,
        }
    }

    /// The canonical `.logic.ron` text of the document (the definition's pretty RON).
    ///
    /// # Errors
    ///
    /// The definition cannot be written as RON.
    pub fn to_ron(&self) -> Result<String, LogicGraphError> {
        self.to_def().to_ron()
    }

    /// Parses `.logic.ron` text into a document laid out by `positions`.
    ///
    /// # Errors
    ///
    /// The text is not a readable version-1 logic graph.
    pub fn from_ron(text: &str, positions: &BTreeMap<String, [f32; 2]>) -> Result<Self, LogicGraphError> {
        Ok(Self::from_def(&LogicGraphDef::from_ron(text)?, positions))
    }

    /// The positions of every node a rule reaches, by layout key (what the sidecar stores).
    #[must_use]
    pub fn layout(&self) -> BTreeMap<String, [f32; 2]> {
        let mut out = BTreeMap::new();
        for chain in self.chains() {
            let Some(rule) = self.rule_name(chain.event) else {
                continue;
            };
            let pos = |id: &NodeId| self.nodes.get(id).map(|n| n.pos);
            if let Some(p) = pos(&chain.event) {
                out.insert(event_key(rule), p);
            }
            for (i, id) in chain.conditions.iter().enumerate() {
                if let Some(p) = pos(id) {
                    out.insert(condition_key(rule, i), p);
                }
            }
            for (i, id) in chain.actions.iter().enumerate() {
                if let Some(p) = pos(id) {
                    out.insert(action_key(rule, i), p);
                }
            }
        }
        out
    }
}

/// The editor-only layout sidecar (`x.logic.ron` → `x.logic.layout.ron`): canvas positions by
/// layout key. Not cooked, safe to delete (the canvas falls back to the three-column layout).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LogicLayoutFile {
    /// [`LOGIC_LAYOUT_VERSION`].
    pub format_version: u32,
    /// Positions by layout key.
    pub positions: BTreeMap<String, [f32; 2]>,
}

impl LogicLayoutFile {
    /// The sidecar of a document.
    #[must_use]
    pub fn of(doc: &LogicGraphDoc) -> Self {
        Self {
            format_version: LOGIC_LAYOUT_VERSION,
            positions: doc.layout(),
        }
    }

    /// Pretty RON.
    ///
    /// # Errors
    ///
    /// The layout cannot be written.
    pub fn to_ron(&self) -> Result<String, String> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| e.to_string())
    }

    /// Reads a sidecar; text that does not parse, a newer version or non-finite positions give no
    /// positions (never an error: the sidecar is optional).
    #[must_use]
    pub fn positions_from_ron(text: &str) -> BTreeMap<String, [f32; 2]> {
        ron::from_str::<Self>(text)
            .ok()
            .filter(|l| l.format_version <= LOGIC_LAYOUT_VERSION)
            .map(|l| {
                l.positions
                    .into_iter()
                    .filter(|(_, p)| p.iter().all(|v| v.is_finite()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The sidecar path of a graph file: `x.logic.ron` → `x.logic.layout.ron`.
#[must_use]
pub fn layout_path(graph: &std::path::Path) -> std::path::PathBuf {
    let name = graph
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".ron").unwrap_or(&name);
    graph.with_file_name(format!("{stem}.layout.ron"))
}

/// One structural edit of a logic graph document. Reads from JSON as `{ "op": "add_event",
/// "rule": "open", "event": { "EntityEvent": "use" } }` (`snake_case` op names; enum fields use the
/// asset's spelling: `"event": "Start"`, `{ "Timer": { "every_ticks": 60 } }`, `"condition": { "Key":
/// "locked" }`, `"value": { "Literal": { "Int": 1 } }` or `{ "BbRef": "local/x" }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogicOp {
    /// Adds a rule head (an event node), appended to the rule order.
    AddEvent {
        /// The rule's name.
        rule: String,
        /// The event arm.
        event: LogicEvent,
        /// Priority (default 0).
        #[serde(default)]
        priority: i32,
        /// Cooldown in fixed steps (default 0).
        #[serde(default)]
        cooldown_ticks: u32,
        /// Use this id (must be unused); default the document's next id.
        #[serde(default)]
        id: Option<NodeId>,
        /// Canvas position; default next to the last rule.
        #[serde(default)]
        at: Option<[f32; 2]>,
    },
    /// Adds a condition node (unwired).
    AddCondition {
        /// Its condition (default `Always`).
        #[serde(default)]
        condition: Condition,
        /// Use this id (must be unused).
        #[serde(default)]
        id: Option<NodeId>,
        /// Canvas position.
        #[serde(default)]
        at: Option<[f32; 2]>,
    },
    /// Adds an action node (unwired).
    AddAction {
        /// Its task path.
        task: String,
        /// Its parameters.
        #[serde(default)]
        set: BTreeMap<String, Binding>,
        /// Use this id (must be unused).
        #[serde(default)]
        id: Option<NodeId>,
        /// Canvas position.
        #[serde(default)]
        at: Option<[f32; 2]>,
    },
    /// Removes a node and every wire touching it.
    RemoveNode {
        /// The node.
        node: NodeId,
    },
    /// Moves a node on the canvas.
    MoveNode {
        /// The node.
        node: NodeId,
        /// Its new position.
        at: [f32; 2],
    },
    /// Wires `from`'s output to `to`'s input (pin types checked; both pins must be free).
    Connect {
        /// The source node.
        from: NodeId,
        /// The target node.
        to: NodeId,
    },
    /// Removes the wire `from → to`.
    Disconnect {
        /// The source node.
        from: NodeId,
        /// The target node.
        to: NodeId,
    },
    /// Changes an event node's fields (each one given).
    SetEvent {
        /// The event node.
        node: NodeId,
        /// Rename the rule.
        #[serde(default)]
        rule: Option<String>,
        /// The event arm.
        #[serde(default)]
        event: Option<LogicEvent>,
        /// Priority.
        #[serde(default)]
        priority: Option<i32>,
        /// Cooldown.
        #[serde(default)]
        cooldown_ticks: Option<u32>,
    },
    /// Replaces a condition node's condition tree.
    SetCondition {
        /// The condition node.
        node: NodeId,
        /// The new tree.
        condition: Condition,
    },
    /// Re-wraps a condition node's terms as `all`, `any`, `not` or `leaf`.
    SetConditionMode {
        /// The condition node.
        node: NodeId,
        /// The mode.
        mode: CondMode,
    },
    /// Changes an action node's task path.
    SetTask {
        /// The action node.
        node: NodeId,
        /// The task path.
        task: String,
    },
    /// Sets (or with no `value`, removes) one parameter of an action node.
    SetBinding {
        /// The action node.
        node: NodeId,
        /// The parameter name.
        key: String,
        /// The binding; absent removes the parameter.
        #[serde(default)]
        value: Option<Binding>,
    },
    /// Sets (or with no `default`, removes) a graph local.
    SetLocal {
        /// The local's name.
        name: String,
        /// Its initial value; absent removes it.
        #[serde(default)]
        default: Option<Literal>,
    },
    /// Sets (or with no `default`, removes) a typed entity variable (`.logic.ron` v2, ADR-8900).
    SetEntityVar {
        /// The entity key.
        name: String,
        /// Its type and default; absent removes the declaration.
        #[serde(default)]
        default: Option<Literal>,
    },
    /// Sets (or with no `default`, removes) a typed world variable of a declared board (v2).
    SetWorldVar {
        /// The world board.
        board: String,
        /// The key on that board.
        name: String,
        /// Its type and default; absent removes the declaration.
        #[serde(default)]
        default: Option<Literal>,
    },
    /// Changes the graph's description, seed or declared world blackboards.
    SetGraph {
        /// The description.
        #[serde(default)]
        description: Option<String>,
        /// The timer-jitter seed.
        #[serde(default)]
        seed: Option<u64>,
        /// The declared world blackboards (spec 102 § 5, phase 3), replacing the list; bad names
        /// show up as validation issues.
        #[serde(default)]
        world: Option<Vec<String>>,
    },
    /// Moves a rule to `index` in the declaration order (clamped to the end).
    MoveRule {
        /// The rule's event node.
        node: NodeId,
        /// The new position.
        index: usize,
    },
}

/// An operation that does not fit the document: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicOpError {
    /// `unknown_node`, `duplicate_id`, `wrong_kind`, `pin_type`, `pin_busy`, `cycle`, `no_wire` or
    /// `bad_value`.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl LogicOpError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A failed batch: which operation failed and why. The document is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicBatchError {
    /// The failing operation's position in the batch.
    pub op_index: usize,
    /// Why it failed.
    pub error: LogicOpError,
}

fn finite(at: [f32; 2]) -> Result<[f32; 2], LogicOpError> {
    if at.iter().all(|v| v.is_finite()) {
        Ok(at)
    } else {
        Err(LogicOpError::new("bad_value", "a position must be finite"))
    }
}

fn fresh_id(doc: &LogicGraphDoc, id: Option<NodeId>) -> Result<NodeId, LogicOpError> {
    match id {
        Some(0) => Err(LogicOpError::new("bad_value", "node id 0 is reserved")),
        Some(id) if doc.nodes.contains_key(&id) => Err(LogicOpError::new(
            "duplicate_id",
            format!("node {id} already exists"),
        )),
        Some(id) => Ok(id),
        None => Ok(doc.next_id),
    }
}

fn add_node(doc: &mut LogicGraphDoc, id: Option<NodeId>, node: LogicNode) -> Result<NodeId, LogicOpError> {
    let id = fresh_id(doc, id)?;
    doc.nodes.insert(id, node);
    doc.next_id = doc.next_id.max(id.saturating_add(1));
    Ok(id)
}

fn node_mut(doc: &mut LogicGraphDoc, node: NodeId) -> Result<&mut LogicNode, LogicOpError> {
    doc.nodes
        .get_mut(&node)
        .ok_or_else(|| LogicOpError::new("unknown_node", format!("no node {node}")))
}

fn wrong_kind(node: NodeId, want: &str, got: &LogicNodeKind) -> LogicOpError {
    LogicOpError::new(
        "wrong_kind",
        format!("node {node} is an {} node, not an {want} node", got.name()),
    )
}

/// The default position of a new node: right of the last node, or the origin.
fn default_at(doc: &LogicGraphDoc, column: f32) -> [f32; 2] {
    let row = doc.rules.len() as f32 * ROW_HEIGHT;
    [column * COLUMN_WIDTH, row]
}

/// Applies one operation; returns the id of a node it added.
///
/// # Errors
///
/// The operation does not fit the document (the document may be partly changed: use
/// [`apply_ops`] for all-or-nothing batches).
pub fn apply_op(doc: &mut LogicGraphDoc, op: &LogicOp) -> Result<Option<NodeId>, LogicOpError> {
    match op {
        LogicOp::AddEvent {
            rule,
            event,
            priority,
            cooldown_ticks,
            id,
            at,
        } => {
            let pos = finite(at.unwrap_or_else(|| default_at(doc, 0.0)))?;
            let id = add_node(
                doc,
                *id,
                LogicNode {
                    kind: LogicNodeKind::Event {
                        rule: rule.clone(),
                        event: event.clone(),
                        priority: *priority,
                        cooldown_ticks: *cooldown_ticks,
                    },
                    pos,
                },
            )?;
            doc.rules.push(id);
            Ok(Some(id))
        }
        LogicOp::AddCondition { condition, id, at } => {
            let pos = finite(at.unwrap_or_else(|| default_at(doc, 1.0)))?;
            let id = add_node(
                doc,
                *id,
                LogicNode {
                    kind: LogicNodeKind::Condition {
                        condition: condition.clone(),
                    },
                    pos,
                },
            )?;
            Ok(Some(id))
        }
        LogicOp::AddAction { task, set, id, at } => {
            let pos = finite(at.unwrap_or_else(|| default_at(doc, 2.0)))?;
            let id = add_node(
                doc,
                *id,
                LogicNode {
                    kind: LogicNodeKind::Action {
                        task: task.clone(),
                        set: set.clone(),
                    },
                    pos,
                },
            )?;
            Ok(Some(id))
        }
        LogicOp::RemoveNode { node } => {
            if doc.nodes.remove(node).is_none() {
                return Err(LogicOpError::new("unknown_node", format!("no node {node}")));
            }
            doc.wires.retain(|from, to| from != node && to != node);
            doc.rules.retain(|r| r != node);
            Ok(None)
        }
        LogicOp::MoveNode { node, at } => {
            let at = finite(*at)?;
            node_mut(doc, *node)?.pos = at;
            Ok(None)
        }
        LogicOp::Connect { from, to } => {
            connect(doc, *from, *to)?;
            Ok(None)
        }
        LogicOp::Disconnect { from, to } => {
            if doc.wires.get(from) != Some(to) {
                return Err(LogicOpError::new("no_wire", format!("no wire {from} -> {to}")));
            }
            doc.wires.remove(from);
            Ok(None)
        }
        LogicOp::SetEvent {
            node,
            rule,
            event,
            priority,
            cooldown_ticks,
        } => {
            let n = node_mut(doc, *node)?;
            let LogicNodeKind::Event {
                rule: r,
                event: e,
                priority: p,
                cooldown_ticks: c,
            } = &mut n.kind
            else {
                return Err(wrong_kind(*node, "event", &n.kind));
            };
            if let Some(v) = rule {
                r.clone_from(v);
            }
            if let Some(v) = event {
                *e = v.clone();
            }
            if let Some(v) = priority {
                *p = *v;
            }
            if let Some(v) = cooldown_ticks {
                *c = *v;
            }
            Ok(None)
        }
        LogicOp::SetCondition { node, condition } => {
            let n = node_mut(doc, *node)?;
            let LogicNodeKind::Condition { condition: c } = &mut n.kind else {
                return Err(wrong_kind(*node, "condition", &n.kind));
            };
            *c = condition.clone();
            Ok(None)
        }
        LogicOp::SetConditionMode { node, mode } => {
            let n = node_mut(doc, *node)?;
            let LogicNodeKind::Condition { condition: c } = &mut n.kind else {
                return Err(wrong_kind(*node, "condition", &n.kind));
            };
            *c = with_mode(c, *mode);
            Ok(None)
        }
        LogicOp::SetTask { node, task } => {
            let n = node_mut(doc, *node)?;
            let LogicNodeKind::Action { task: t, .. } = &mut n.kind else {
                return Err(wrong_kind(*node, "action", &n.kind));
            };
            t.clone_from(task);
            Ok(None)
        }
        LogicOp::SetBinding { node, key, value } => {
            let n = node_mut(doc, *node)?;
            let LogicNodeKind::Action { set, .. } = &mut n.kind else {
                return Err(wrong_kind(*node, "action", &n.kind));
            };
            if key.is_empty() {
                return Err(LogicOpError::new(
                    "bad_value",
                    "a parameter name must not be empty",
                ));
            }
            match value {
                Some(v) => {
                    set.insert(key.clone(), v.clone());
                }
                None => {
                    set.remove(key);
                }
            }
            Ok(None)
        }
        LogicOp::SetLocal { name, default } => {
            match default {
                Some(v) => match doc.locals.iter_mut().find(|l| &l.name == name) {
                    Some(local) => local.default = v.clone(),
                    None => doc.locals.push(LocalDef {
                        name: name.clone(),
                        default: v.clone(),
                    }),
                },
                None => doc.locals.retain(|l| &l.name != name),
            }
            Ok(None)
        }
        LogicOp::SetEntityVar { name, default } => {
            match default {
                Some(v) => match doc.entity_vars.iter_mut().find(|e| &e.name == name) {
                    Some(var) => var.default = v.clone(),
                    None => doc.entity_vars.push(EntityVarDef {
                        name: name.clone(),
                        default: v.clone(),
                    }),
                },
                None => doc.entity_vars.retain(|e| &e.name != name),
            }
            Ok(None)
        }
        LogicOp::SetWorldVar { board, name, default } => {
            match default {
                Some(v) => match doc
                    .world_vars
                    .iter_mut()
                    .find(|w| &w.board == board && &w.name == name)
                {
                    Some(var) => var.default = v.clone(),
                    None => doc.world_vars.push(WorldVarDef {
                        board: board.clone(),
                        name: name.clone(),
                        default: v.clone(),
                    }),
                },
                None => doc.world_vars.retain(|w| !(&w.board == board && &w.name == name)),
            }
            Ok(None)
        }
        LogicOp::SetGraph {
            description,
            seed,
            world,
        } => {
            if let Some(d) = description {
                doc.description.clone_from(d);
            }
            if let Some(s) = seed {
                doc.seed = *s;
            }
            if let Some(w) = world {
                doc.world.clone_from(w);
            }
            Ok(None)
        }
        LogicOp::MoveRule { node, index } => {
            let Some(at) = doc.rules.iter().position(|r| r == node) else {
                return Err(LogicOpError::new(
                    "unknown_node",
                    format!("node {node} is not a rule head"),
                ));
            };
            let id = doc.rules.remove(at);
            let index = (*index).min(doc.rules.len());
            doc.rules.insert(index, id);
            Ok(None)
        }
    }
}

/// Whether `from → to` may be wired: pin types (event out → condition or action; condition out →
/// condition or action; action out → action; nothing into an event), free pins and no cycle.
///
/// # Errors
///
/// `unknown_node`, `pin_type`, `pin_busy` or `cycle`.
pub fn check_wire(doc: &LogicGraphDoc, from: NodeId, to: NodeId) -> Result<(), LogicOpError> {
    let kind = |id: NodeId| {
        doc.nodes
            .get(&id)
            .map(|n| &n.kind)
            .ok_or_else(|| LogicOpError::new("unknown_node", format!("no node {id}")))
    };
    let (a, b) = (kind(from)?, kind(to)?);
    let fits = matches!(
        (a, b),
        (
            LogicNodeKind::Event { .. } | LogicNodeKind::Condition { .. },
            LogicNodeKind::Condition { .. } | LogicNodeKind::Action { .. }
        ) | (LogicNodeKind::Action { .. }, LogicNodeKind::Action { .. })
    );
    if !fits {
        return Err(LogicOpError::new(
            "pin_type",
            format!(
                "an {} output cannot feed an {} input (events lead, conditions come before actions)",
                a.name(),
                b.name()
            ),
        ));
    }
    if from == to {
        return Err(LogicOpError::new(
            "cycle",
            format!("node {from} cannot feed itself"),
        ));
    }
    if let Some(next) = doc.wires.get(&from) {
        return Err(LogicOpError::new(
            "pin_busy",
            format!("node {from} already feeds node {next}: disconnect it first"),
        ));
    }
    if let Some(prev) = doc.wire_into(to) {
        return Err(LogicOpError::new(
            "pin_busy",
            format!("node {to} is already fed by node {prev}: disconnect it first"),
        ));
    }
    // A cycle would need `to` to reach `from` downstream.
    let mut at = to;
    let mut steps = 0usize;
    while let Some(&next) = doc.wires.get(&at) {
        if next == from {
            return Err(LogicOpError::new(
                "cycle",
                format!("wiring {from} -> {to} closes a loop"),
            ));
        }
        at = next;
        steps += 1;
        if steps > doc.nodes.len() {
            break;
        }
    }
    Ok(())
}

fn connect(doc: &mut LogicGraphDoc, from: NodeId, to: NodeId) -> Result<(), LogicOpError> {
    check_wire(doc, from, to)?;
    doc.wires.insert(from, to);
    Ok(())
}

/// Applies `ops` in order, all or nothing; returns the ids of the nodes they added.
///
/// # Errors
///
/// The first operation that does not fit; `doc` is unchanged.
pub fn apply_ops(doc: &mut LogicGraphDoc, ops: &[LogicOp]) -> Result<Vec<NodeId>, LogicBatchError> {
    let mut work = doc.clone();
    let mut created = Vec::new();
    for (op_index, op) in ops.iter().enumerate() {
        match apply_op(&mut work, op) {
            Ok(Some(id)) => created.push(id),
            Ok(None) => {}
            Err(error) => return Err(LogicBatchError { op_index, error }),
        }
    }
    *doc = work;
    Ok(created)
}

/// How bad an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicSeverity {
    /// The graph does not compile; saving is refused.
    Error,
    /// Worth a look; the graph still compiles.
    Warning,
}

/// One validation problem, with the stable code of spec 102 § 6 / § 10.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicIssue {
    /// `dangling_wire`, `duplicate_name`, `too_many_rules`, `too_many_actions`, `condition_too_deep`,
    /// `unknown_task`, `unknown_scope`, `bad_value`, `bad_op`, `missing_param`,
    /// `unsupported_feature` or `unsupported_version`.
    pub code: &'static str,
    /// Error or warning.
    pub severity: LogicSeverity,
    /// The node it is about, when one is.
    pub node: Option<NodeId>,
    /// The rule it is about, when one is.
    pub rule: Option<String>,
    /// A sentence for people.
    pub message: String,
}

/// Whether `issues` has no error.
#[must_use]
pub fn is_valid(issues: &[LogicIssue]) -> bool {
    issues.iter().all(|i| i.severity != LogicSeverity::Error)
}

fn mentions_key(c: &Condition, key: &str) -> bool {
    match c {
        Condition::Key(k) | Condition::Has(k) => k == key,
        Condition::Compare { key: k, .. } | Condition::Text { key: k, .. } => k == key,
        Condition::Not(inner) => mentions_key(inner, key),
        Condition::All(v) | Condition::Any(v) => v.iter().any(|c| mentions_key(c, key)),
        Condition::Always => false,
    }
}

/// The node of `chain` a compile error points at: the action with the named task, the condition or
/// action that mentions the key, else the event node.
fn blame(doc: &LogicGraphDoc, chain: &RuleChain, err: &LogicGraphError) -> NodeId {
    let kind = |id: NodeId| doc.nodes.get(&id).map(|n| &n.kind);
    let found = match err {
        LogicGraphError::UnknownTask { task, .. } => chain
            .actions
            .iter()
            .find(|id| matches!(kind(**id), Some(LogicNodeKind::Action { task: t, .. }) if t == task)),
        LogicGraphError::UnknownScope { key, .. } => chain
            .conditions
            .iter()
            .find(|id| matches!(kind(**id), Some(LogicNodeKind::Condition { condition }) if mentions_key(condition, key)))
            .or_else(|| {
                chain.actions.iter().find(|id| {
                    matches!(kind(**id), Some(LogicNodeKind::Action { set, .. })
                        if set.values().any(|b| matches!(b, Binding::BbRef(k) if k == key))
                            || set.get("key").is_some_and(|b| matches!(b, Binding::Literal(Literal::String(k)) if k == key)))
                })
            }),
        LogicGraphError::ConditionTooDeep(_) => chain.conditions.first(),
        LogicGraphError::BadOp { .. } | LogicGraphError::MissingParam { .. } => chain.actions.first(),
        _ => None,
    };
    found.copied().unwrap_or(chain.event)
}

/// Every problem of a document. Tasks are checked against `registry` when given; without one only
/// the built-in `logic::*` tasks are known and other task paths are `unknown_task` warnings (the
/// editor has no game registry loaded). Each rule is compiled on its own, so one bad rule does not
/// hide another's problems.
#[must_use]
pub fn validate(doc: &LogicGraphDoc, registry: Option<&BtTaskRegistry>) -> Vec<LogicIssue> {
    let mut issues = Vec::new();
    for id in doc.dangling() {
        let kind = doc.nodes.get(&id).map_or("node", |n| n.kind.name());
        issues.push(LogicIssue {
            code: "dangling_wire",
            severity: LogicSeverity::Error,
            node: Some(id),
            rule: None,
            message: format!("the {kind} node {id} is not wired to any event"),
        });
    }
    if doc.rules.len() > MAX_RULES {
        issues.push(LogicIssue {
            code: "too_many_rules",
            severity: LogicSeverity::Error,
            node: None,
            rule: None,
            message: format!(
                "the graph has {} rules (the limit is {MAX_RULES})",
                doc.rules.len()
            ),
        });
    }
    let mut names = BTreeSet::new();
    for &event in &doc.rules {
        let name = doc.rule_name(event).unwrap_or_default();
        if !name.is_empty() && !names.insert(name) {
            issues.push(LogicIssue {
                code: "duplicate_name",
                severity: LogicSeverity::Error,
                node: Some(event),
                rule: Some(name.to_string()),
                message: format!("two rules are named {name:?}"),
            });
        }
    }
    let empty = BtTaskRegistry::default();
    let registry = registry.unwrap_or(&empty);
    let def = doc.to_def();
    // Graph-level checks first (version, world list, locals), with no rules.
    let header = LogicGraphDef {
        rules: Vec::new(),
        ..def.clone()
    };
    let header_ok = match LogicGraph::compile(&header, registry) {
        Ok(_) => true,
        Err(err) => {
            issues.push(LogicIssue {
                code: err.reason(),
                severity: LogicSeverity::Error,
                node: None,
                rule: err.rule().map(str::to_string),
                message: err.to_string(),
            });
            false
        }
    };
    for (chain, rule) in doc.chains().iter().zip(&def.rules) {
        // A broken header (locals or world list) is reported once above; each rule then compiles
        // without it, so the header's problem is not repeated on every rule.
        let single = LogicGraphDef {
            locals: if header_ok { def.locals.clone() } else { Vec::new() },
            world: if header_ok { def.world.clone() } else { Vec::new() },
            entity_vars: if header_ok {
                def.entity_vars.clone()
            } else {
                Vec::new()
            },
            world_vars: if header_ok {
                def.world_vars.clone()
            } else {
                Vec::new()
            },
            rules: vec![rule.clone()],
            ..LogicGraphDef::new()
        };
        if let Err(err) = LogicGraph::compile(&single, registry) {
            let unregistered = matches!(err, LogicGraphError::UnknownTask { .. }) && registry.is_empty();
            issues.push(LogicIssue {
                code: err.reason(),
                severity: if unregistered {
                    LogicSeverity::Warning
                } else {
                    LogicSeverity::Error
                },
                node: Some(blame(doc, chain, &err)),
                rule: Some(rule.name.clone()),
                message: if unregistered {
                    format!("{err} (no task registry is loaded, so it is not checked)")
                } else {
                    err.to_string()
                },
            });
        }
    }
    issues
}

#[cfg(test)]
mod tests;
