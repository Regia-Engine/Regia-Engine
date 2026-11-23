// SPDX-License-Identifier: MPL-2.0
//! The conversation as a graph: blocks of steps between labels, with an edge for every way the
//! runner can leave a block (ADR-3298).
//!
//! A *block* starts at step 0, at every `Label`, and after every step that leaves (`Goto`, `If`,
//! `Choice`, `End`). It holds `Label`, `Line` and `Event` steps and may end with a `Goto`. `If`,
//! `Choice` and `End` are nodes of their own, so a writer sees each branch point. Edges:
//! `next` (falls through to the following step), `goto`, `then`, `else` and `option`. A jump to a
//! label that does not exist is an edge with no target (`to: None`), drawn as a dangling arrow.
//!
//! Node ids are stable names, so layout positions survive edits that do not change the
//! structure: `label:<name>` for a block that starts at a label, `line:<id>` for one that starts
//! at a line, and `<kind>:<n>` (the n-th node of that kind without a natural name) otherwise.

use std::collections::BTreeMap;

use regia_framework::conversation::{ConversationAsset, Step};
use serde_json::{Value, json};

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeKind {
    /// Labels, lines and events, possibly ending with a `Goto`.
    Block,
    /// A `Choice` step.
    Choice,
    /// An `If` step.
    If,
    /// An `End` step.
    End,
}

impl NodeKind {
    /// The lowercase name the agent uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Choice => "choice",
            Self::If => "if",
            Self::End => "end",
        }
    }
}

/// One node of the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    /// The stable id (see the module docs).
    pub id: String,
    /// The kind.
    pub kind: NodeKind,
    /// The first step index (into the effective steps).
    pub first: usize,
    /// One past the last step index.
    pub end: usize,
    /// The label the node starts at, if any.
    pub label: Option<String>,
}

impl GraphNode {
    /// Whether the node holds step `step`.
    #[must_use]
    pub fn contains(&self, step: usize) -> bool {
        (self.first..self.end).contains(&step)
    }
}

/// What kind of jump an edge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// Falls through to the following step.
    Next,
    /// A `Goto`.
    Goto,
    /// The `then` branch of an `If`.
    Then,
    /// The `else` branch of an `If`.
    Else,
    /// A choice option.
    Option,
}

impl EdgeKind {
    /// The lowercase name the agent uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Goto => "goto",
            Self::Then => "then",
            Self::Else => "else",
            Self::Option => "option",
        }
    }
}

/// One edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    /// The source node index.
    pub from: usize,
    /// The target node index; `None` when the label does not exist.
    pub to: Option<usize>,
    /// The kind.
    pub kind: EdgeKind,
    /// The step that jumps (the `Goto`, `If` or `Choice`; for `next` the last step of `from`).
    pub step: usize,
    /// The target label (empty for `next`).
    pub label: String,
    /// The option index for `option` edges.
    pub option: Option<usize>,
    /// The option's condition, or the `If` condition for `then`/`else`.
    pub cond: Option<String>,
    /// The option's text key.
    pub text: Option<String>,
}

/// The graph of one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConvGraph {
    /// The effective steps the graph was built from.
    pub steps: Vec<Step>,
    /// The nodes in step order; node 0 is the entry.
    pub nodes: Vec<GraphNode>,
    /// The edges, by source node then step then option.
    pub edges: Vec<GraphEdge>,
}

fn is_exit(step: &Step) -> bool {
    matches!(
        step,
        Step::Goto(_) | Step::If { .. } | Step::Choice { .. } | Step::End
    )
}

impl ConvGraph {
    /// Builds the graph of `asset`'s effective steps.
    #[must_use]
    pub fn build(asset: &ConversationAsset) -> Self {
        let steps = asset.effective_steps();
        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut counters: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut used: BTreeMap<String, usize> = BTreeMap::new();
        let mut name = |base: String, kind: &'static str, used: &mut BTreeMap<String, usize>| -> String {
            // A natural name used twice (a duplicate label) gets a `#n` suffix.
            let base = if base.is_empty() {
                let n = counters.entry(kind).or_insert(0);
                *n += 1;
                format!("{kind}:{n}")
            } else {
                base
            };
            let seen = used.entry(base.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                base
            } else {
                format!("{base}#{seen}")
            }
        };
        let mut i = 0;
        while i < steps.len() {
            let (kind, end) = match &steps[i] {
                Step::If { .. } => (NodeKind::If, i + 1),
                Step::Choice { .. } => (NodeKind::Choice, i + 1),
                Step::End => (NodeKind::End, i + 1),
                _ => {
                    // A block: up to the next label (not the first step), up to and including a
                    // Goto, or up to an If, Choice or End.
                    let mut j = i;
                    loop {
                        let step = &steps[j];
                        if j > i && matches!(step, Step::Label(_)) {
                            break;
                        }
                        if matches!(step, Step::If { .. } | Step::Choice { .. } | Step::End) {
                            break;
                        }
                        j += 1;
                        if matches!(step, Step::Goto(_)) || j >= steps.len() {
                            break;
                        }
                    }
                    (NodeKind::Block, j)
                }
            };
            let label = match &steps[i] {
                Step::Label(l) => Some(l.clone()),
                _ => None,
            };
            let natural = match (&label, kind) {
                (Some(l), _) => format!("label:{l}"),
                (None, NodeKind::Block) => steps[i..end]
                    .iter()
                    .find_map(|s| match s {
                        Step::Line(id) => Some(format!("line:{id}")),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let id = name(natural, kind.name(), &mut used);
            nodes.push(GraphNode {
                id,
                kind,
                first: i,
                end,
                label,
            });
            i = end;
        }
        // Labels resolve to their first occurrence, as the runner does.
        let mut label_node: BTreeMap<&str, usize> = BTreeMap::new();
        for (n, node) in nodes.iter().enumerate() {
            if let Some(l) = &node.label {
                label_node.entry(l.as_str()).or_insert(n);
            }
        }
        let node_of_step = |step: usize| nodes.iter().position(|n| n.contains(step));
        let mut edges = Vec::new();
        for (n, node) in nodes.iter().enumerate() {
            let last = node.end - 1;
            let jump = |kind, label: &str, step, option, cond: Option<String>, text| GraphEdge {
                from: n,
                to: label_node.get(label).copied(),
                kind,
                step,
                label: label.to_string(),
                option,
                cond,
                text,
            };
            match &steps[last] {
                Step::Goto(l) => edges.push(jump(EdgeKind::Goto, l, last, None, None, None)),
                Step::If { cond, then, else_ } => {
                    edges.push(jump(EdgeKind::Then, then, last, None, Some(cond.clone()), None));
                    edges.push(jump(EdgeKind::Else, else_, last, None, Some(cond.clone()), None));
                }
                Step::Choice { options, .. } => {
                    for (k, o) in options.iter().enumerate() {
                        edges.push(jump(
                            EdgeKind::Option,
                            &o.goto,
                            last,
                            Some(k),
                            o.cond.clone(),
                            Some(o.text.clone()),
                        ));
                    }
                }
                Step::End => {}
                _ => {
                    if node.end < steps.len() && !is_exit(&steps[last]) {
                        edges.push(GraphEdge {
                            from: n,
                            to: node_of_step(node.end),
                            kind: EdgeKind::Next,
                            step: last,
                            label: String::new(),
                            option: None,
                            cond: None,
                            text: None,
                        });
                    }
                }
            }
        }
        Self { steps, nodes, edges }
    }

    /// The node index with that id.
    #[must_use]
    pub fn node_index(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    /// The node holding step `step`.
    #[must_use]
    pub fn node_of_step(&self, step: usize) -> Option<usize> {
        self.nodes.iter().position(|n| n.contains(step))
    }

    /// The node playing line `line` (its first `Line` step).
    #[must_use]
    pub fn node_of_line(&self, line: &str) -> Option<usize> {
        self.steps
            .iter()
            .position(|s| matches!(s, Step::Line(id) if id == line))
            .and_then(|s| self.node_of_step(s))
    }

    /// The node a label starts.
    #[must_use]
    pub fn node_of_label(&self, label: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.label.as_deref() == Some(label))
    }

    /// The structural signature the layout depends on (ids, kinds and edges, no text).
    #[must_use]
    pub fn structure_key(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            out.push_str(&n.id);
            out.push(';');
        }
        out.push('|');
        for e in &self.edges {
            out.push_str(&format!("{}>{:?}:{};", e.from, e.to, e.kind.name()));
        }
        out
    }

    /// The JSON of one step, as the agent shows it.
    #[must_use]
    pub fn step_json(asset: &ConversationAsset, step: &Step) -> Value {
        match step {
            Step::Line(id) => {
                let line = asset.line(id);
                json!({
                    "kind": "line",
                    "line": id,
                    "speaker": line.map(|l| l.speaker.as_str()),
                    "text": line.map(|l| l.text.as_str()),
                })
            }
            Step::Label(name) => json!({ "kind": "label", "name": name }),
            Step::Goto(label) => json!({ "kind": "goto", "label": label }),
            Step::If { cond, then, else_ } => {
                json!({ "kind": "if", "cond": cond, "then": then, "else": else_ })
            }
            Step::Choice { prompt, options } => json!({
                "kind": "choice",
                "prompt": prompt,
                "options": options
                    .iter()
                    .map(|o| json!({ "text": o.text, "cond": o.cond, "goto": o.goto }))
                    .collect::<Vec<_>>(),
            }),
            Step::Event(name) => json!({ "kind": "event", "name": name }),
            Step::End => json!({ "kind": "end" }),
        }
    }
}
