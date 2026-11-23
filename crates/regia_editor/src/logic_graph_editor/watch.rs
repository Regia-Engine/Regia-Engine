// SPDX-License-Identifier: MPL-2.0
//! Watching a running logic graph from the panel (ADR-8021, spec 102 § 9): the scope snapshot the
//! watch list, hover values and the evaluate box read, the Watch list with its Live toggle, the
//! evaluate box and the pulse on the wires of rules that just fired.
//!
//! The editor never reads the play world outside the pump, so the pump copies the watched graph's
//! scopes (its locals, its entity's `Blackboard` and its declared world boards) into a
//! [`ScopeSnapshot`]: every pump while Live is on, else only when the graph is paused on a tick the
//! snapshot does not hold yet (a running graph with Live off costs nothing). Every read resolves its
//! key or condition with the runtime's own scope rule (`regia_ai::logic::LogicScopeView`), so a
//! watch shows exactly what a rule would see.

use std::collections::BTreeMap;

use regia_ai::Blackboard;
use regia_ai::logic::{LogicGraph, LogicScopeView, LogicWorldBlackboards};
use regia_ai::logic_edit::{LogicGraphDoc, LogicNodeKind, NodeId};
use regia_ai::state_tree::Condition;
use regia_ecs::prelude::*;

/// How many editor frames the wires of a rule that fired keep pulsing.
pub const PULSE_FRAMES: u32 = 30;

/// The most entries a Watch list holds.
pub const MAX_WATCHES: usize = 64;

/// The watched graph's scopes, copied by the pump.
#[derive(Debug, Clone)]
pub struct ScopeSnapshot {
    /// The graph entity (bits).
    pub entity: u64,
    /// The graph tick the copy is from.
    pub ticks: u64,
    /// Whether the graph was paused when copied.
    pub paused: bool,
    /// The boards the graph declares.
    pub world_names: Vec<String>,
    /// Its locals.
    pub locals: Blackboard,
    /// Its entity's `Blackboard`.
    pub entity_vars: Blackboard,
    /// Its declared world boards.
    pub boards: LogicWorldBlackboards,
}

impl ScopeSnapshot {
    /// Copies `graph`'s scopes on `entity` in `play`.
    #[must_use]
    pub fn of(graph: &LogicGraph, play: &World, entity: Entity) -> Self {
        let view = graph.scope_view(play, entity);
        let mut boards = LogicWorldBlackboards::default();
        if let Some(all) = view.boards {
            for name in view.world {
                if let Some(board) = all.get(name) {
                    boards.boards.insert(name.clone(), board.clone());
                }
            }
        }
        Self {
            entity: entity.to_bits(),
            ticks: graph.ticks(),
            paused: graph.is_paused(),
            world_names: view.world.to_vec(),
            locals: view.locals.clone(),
            entity_vars: view.entity.clone(),
            boards,
        }
    }

    /// The scopes, for reads.
    #[must_use]
    pub fn view(&self) -> LogicScopeView<'_> {
        LogicScopeView {
            locals: &self.locals,
            entity: &self.entity_vars,
            boards: Some(&self.boards),
            world: &self.world_names,
        }
    }

    /// The value of `key` as text (`unset` when the scope holds none).
    ///
    /// # Errors
    ///
    /// The key names a scope the graph does not declare (`code: message`).
    pub fn read(&self, key: &str) -> Result<String, String> {
        self.view()
            .read(key)
            .map(|v| v.map_or_else(|| "unset".to_string(), |v| format!("{v:?}")))
            .map_err(|e| format!("{}: {e}", e.reason()))
    }

    /// `condition` evaluated as a rule guard of the graph would.
    ///
    /// # Errors
    ///
    /// The condition does not compile in the graph (`code: message`).
    pub fn evaluate(&self, condition: &Condition) -> Result<bool, String> {
        self.view()
            .evaluate(condition)
            .map_err(|e| format!("{}: {e}", e.reason()))
    }
}

/// One Watch list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchItem {
    /// A variable: `local/x`, `world/<board>/x`, `entity/x` or a bare entity key.
    Key(String),
    /// A condition node's result.
    Condition(NodeId),
}

impl WatchItem {
    /// The entry as the list shows it.
    #[must_use]
    pub fn label(&self, doc: &LogicGraphDoc) -> String {
        match self {
            Self::Key(k) => k.clone(),
            Self::Condition(id) => match doc.nodes.get(id).map(|n| &n.kind) {
                Some(kind @ LogicNodeKind::Condition { .. }) => {
                    format!("#{id} {}", super::panel::node_title(kind))
                }
                _ => format!("#{id} (not a condition)"),
            },
        }
    }
}

/// A watch entry's value: the text, or why it cannot be read.
pub type WatchValue = Result<String, String>;

/// The Watch list (inside [`super::LogicEditor`]).
#[derive(Debug, Clone, Default)]
pub struct WatchList {
    /// The entries, in the order they were pinned.
    pub items: Vec<WatchItem>,
    /// Refresh the snapshot every editor frame while the game plays (`false`: only when the
    /// watched graph is paused).
    pub live: bool,
    /// The watched graph's scopes, as last copied.
    pub snapshot: Option<ScopeSnapshot>,
    /// Each entry's value at the snapshot, in `items` order.
    pub values: Vec<WatchValue>,
    /// Snapshot copies so far (the cost of watching; a running graph with Live off adds none).
    pub refreshes: u64,
    /// The node the pointer is over in the panel (its hover value is read from the snapshot).
    pub hovered: Option<NodeId>,
    /// The evaluate box's last expression and its result.
    pub evaluation: Option<(String, Result<bool, String>)>,
}

impl WatchList {
    /// Pins `item` (nothing when it is already listed).
    ///
    /// # Errors
    ///
    /// The list is full ([`MAX_WATCHES`]).
    pub fn add(&mut self, item: WatchItem) -> Result<(), String> {
        if self.items.contains(&item) {
            return Ok(());
        }
        if self.items.len() >= MAX_WATCHES {
            return Err(format!("the Watch list holds at most {MAX_WATCHES} entries"));
        }
        self.items.push(item);
        Ok(())
    }

    /// Unpins `item`. Whether it was listed.
    pub fn remove(&mut self, item: &WatchItem) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i != item);
        self.items.len() != before
    }

    /// Recomputes every entry's value from the snapshot (entries read nothing without one).
    pub fn recompute(&mut self, doc: &LogicGraphDoc) {
        let values = self
            .items
            .iter()
            .map(|item| value_of(self.snapshot.as_ref(), doc, item))
            .collect();
        self.values = values;
    }
}

/// `item`'s value in `snapshot`.
pub fn value_of(snapshot: Option<&ScopeSnapshot>, doc: &LogicGraphDoc, item: &WatchItem) -> WatchValue {
    let Some(snapshot) = snapshot else {
        return Err("no value yet: play the game, then pause the graph or turn Live on".into());
    };
    match item {
        WatchItem::Key(key) => snapshot.read(key),
        WatchItem::Condition(id) => match doc.nodes.get(id).map(|n| &n.kind) {
            Some(LogicNodeKind::Condition { condition }) => {
                snapshot.evaluate(condition).map(|b| b.to_string())
            }
            _ => Err(format!("node {id} is not a condition")),
        },
    }
}

/// The hover text of node `id`: a condition's result (or a variable key's value) in `snapshot`.
#[must_use]
pub fn hover_value(snapshot: Option<&ScopeSnapshot>, doc: &LogicGraphDoc, id: NodeId) -> Option<String> {
    let snapshot = snapshot?;
    match doc.nodes.get(&id).map(|n| &n.kind)? {
        LogicNodeKind::Condition { condition } => Some(match snapshot.evaluate(condition) {
            Ok(b) => format!("now: {b} (tick {})", snapshot.ticks),
            Err(e) => format!("cannot evaluate: {e}"),
        }),
        _ => None,
    }
}

/// Parses an evaluate-box expression: the condition syntax of the graph (spec 102 § 4.2: a
/// `Condition` in RON, as the inspector's Condition field takes it).
///
/// # Errors
///
/// The text is not a condition (`parse: message`).
pub fn parse_expression(text: &str) -> Result<Condition, String> {
    ron::from_str::<Condition>(text.trim()).map_err(|e| format!("parse: {e}"))
}

/// Evaluates `text` against the snapshot of a paused graph.
///
/// # Errors
///
/// `not_paused` (no snapshot of a paused graph), `parse`, or the condition's compile code
/// (`unknown_scope`, `condition_too_deep`, `bad_value`), each as `code: message`.
pub fn evaluate(snapshot: Option<&ScopeSnapshot>, text: &str) -> Result<bool, String> {
    let snapshot = snapshot
        .filter(|s| s.paused)
        .ok_or_else(|| "not_paused: pause the watched graph (or hit a breakpoint) first".to_string())?;
    let condition = parse_expression(text)?;
    snapshot.evaluate(&condition)
}

/// The wires that pulse: `(from, to)` → frames left.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pulse {
    /// Pulsing wires and their frames left.
    pub wires: BTreeMap<(NodeId, NodeId), u32>,
    /// The tick whose fires started the current pulses.
    tick: Option<u64>,
    /// Pulse updates so far (only while a graph is watched: an unwatched graph costs none).
    pub updates: u64,
}

impl Pulse {
    /// One editor frame: the wires of the rules that fired on `fired_tick` start pulsing (once
    /// per tick), the others fade by a frame.
    pub fn frame(&mut self, doc: &LogicGraphDoc, fired: &[&str], fired_tick: Option<u64>) {
        self.updates += 1;
        self.wires.retain(|_, left| {
            *left = left.saturating_sub(1);
            *left > 0
        });
        if fired_tick.is_none() || fired_tick == self.tick {
            return;
        }
        self.tick = fired_tick;
        for chain in doc.chains() {
            let Some(rule) = doc.rule_name(chain.event) else {
                continue;
            };
            if !fired.contains(&rule) {
                continue;
            }
            let mut from = chain.event;
            for &to in chain.conditions.iter().chain(&chain.actions) {
                self.wires.insert((from, to), PULSE_FRAMES);
                from = to;
            }
        }
    }

    /// Clears every pulse (no graph watched).
    pub fn clear(&mut self) {
        self.wires.clear();
        self.tick = None;
    }

    /// How strongly the wire `from → to` pulses now: 0 (not) to 1 (just fired).
    #[must_use]
    pub fn strength(&self, from: NodeId, to: NodeId) -> f32 {
        self.wires
            .get(&(from, to))
            .map_or(0.0, |left| *left as f32 / PULSE_FRAMES as f32)
    }
}
