// SPDX-License-Identifier: MPL-2.0
//! Find usages across the project (w13 P-183, ADR-8565): which logic graphs emit or receive an
//! event, which read, write or watch a blackboard key, and where an animation-graph parameter is
//! declared, read by an animation graph, set by a logic action or named by game code.
//!
//! The project usage index is built on demand: every `.logic.ron`, `.animgraph.ron` and `.rs` file
//! under the project root is read, and a file whose content hash is unchanged since the last search
//! keeps its extracted usages (nothing is parsed again). A logic node's context menu and the
//! inspector offer "Find usages of …" for each event and key the node names; the panel's Usages
//! tab lists the answer, and a row opens the graph at the node. The agent twin is
//! `editor.logic_find_usages`.
//!
//! What counts as a use (`how`):
//! - event: `receives` (a rule's event arm, by its inbox name) and `emits` (`logic::EmitEvent`
//!   with a literal name).
//! - key (`entity/x` and bare `x` are the same key; `local/x` is per graph; `world/<board>/x` is
//!   shared): `reads` (conditions, `BbRef` bindings), `writes` (`logic::SetBlackboard` with a
//!   literal key, and a registered task's parameters, which the runtime writes into the entity
//!   blackboard), `watches` (`BlackboardChanged` events), `declares` (graph locals).
//! - param: `declares` and `reads` in animation graphs, `sets` (a registered task's parameter of
//!   that name, or a literal string parameter value naming it) in logic graphs, and `sets` in
//!   code (`set_float("Name", …)` and the other `ParameterSet` setters, by line).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::logic::{BUILTIN_TASKS, Binding, LogicEvent};
use regia_ai::logic_edit::{LogicGraphDoc, LogicNodeKind, NodeId};
use regia_ai::state_tree::{Condition, Literal};
use regia_animation::{AnimGraph, AnimNode, IkTarget};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::LogicEditor;

/// Most directory entries one project scan visits (the material graph usage scan's guard).
pub const SCAN_LIMIT: usize = crate::matgraph_editor::usages::SCAN_LIMIT;

/// The file suffixes the index reads.
pub const INDEXED_SUFFIXES: [&str; 3] = [".logic.ron", ".animgraph.ron", ".rs"];

/// The `ParameterSet` setters a code line may name a parameter with.
const CODE_SETTERS: [&str; 6] = [
    "set_float(\"",
    "set_int(\"",
    "set_bool(\"",
    "set_vec2(\"",
    "set_vec3(\"",
    "set_trigger(\"",
];

/// What a usage search looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsageKind {
    /// An event name (as the graph's inbox receives it).
    Event,
    /// A blackboard key.
    Key,
    /// An animation-graph parameter.
    Param,
}

impl UsageKind {
    /// Every kind.
    pub const ALL: [Self; 3] = [Self::Event, Self::Key, Self::Param];

    /// The protocol name: `event`, `key` or `param`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Key => "key",
            Self::Param => "param",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == text)
    }
}

/// A usage search: a kind and a name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsageQuery {
    /// What is searched.
    pub kind: UsageKind,
    /// The name (a key in any of its spellings: `entity/x` and `x` match each other).
    pub name: String,
}

impl UsageQuery {
    /// A query of `kind` for `name`.
    #[must_use]
    pub fn new(kind: UsageKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }

    /// The menu text: `Find usages of event "go"`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("Find usages of {} \"{}\"", self.kind.name(), self.name)
    }

    fn matches(&self, fact: &Fact) -> bool {
        fact.kind == self.kind
            && match self.kind {
                UsageKind::Key => normalize_key(&self.name) == fact.name,
                _ => self.name == fact.name,
            }
    }
}

/// `entity/x` and bare `x` name the same entity key; other scopes keep their prefix.
#[must_use]
pub fn normalize_key(key: &str) -> String {
    key.strip_prefix("entity/").unwrap_or(key).to_string()
}

/// One use the index extracted from a file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fact {
    kind: UsageKind,
    /// The name (keys normalized).
    name: String,
    how: &'static str,
    node: Option<NodeId>,
    rule: Option<String>,
    line: Option<usize>,
    detail: String,
}

/// One use of the searched item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicUsage {
    /// The file.
    pub path: PathBuf,
    /// `logic` (a `.logic.ron`), `anim` (an `.animgraph.ron`) or `code` (a `.rs`).
    pub doc: &'static str,
    /// `receives`, `emits`, `reads`, `writes`, `watches`, `declares` or `sets`.
    pub how: &'static str,
    /// The logic node (ids as the Logic Graph panel opens the file).
    pub node: Option<NodeId>,
    /// The rule the node belongs to.
    pub rule: Option<String>,
    /// The line (code).
    pub line: Option<usize>,
    /// Where in the file, in words (the node's title, the anim node, the code line).
    pub detail: String,
}

impl LogicUsage {
    /// As protocol JSON (`index` is the row's position in the answer).
    #[must_use]
    pub fn to_json(&self, index: usize) -> Value {
        json!({
            "index": index,
            "path": self.path.display().to_string(),
            "doc": self.doc,
            "how": self.how,
            "node": self.node,
            "rule": self.rule,
            "line": self.line,
            "detail": self.detail,
        })
    }

    /// The row as the Usages tab shows it.
    #[must_use]
    pub fn row_text(&self, root: &Path) -> String {
        let path = self.path.strip_prefix(root).unwrap_or(&self.path).display();
        let at = match (self.node, &self.rule, self.line) {
            (Some(n), Some(rule), _) => format!(" rule {rule} node #{n}"),
            (Some(n), None, _) => format!(" node #{n}"),
            (None, _, Some(line)) => format!(":{line}"),
            _ => String::new(),
        };
        format!("{} {path}{at}: {} ({})", self.doc, self.how, self.detail)
    }
}

/// The last search's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageAnswer {
    /// The search.
    pub query: UsageQuery,
    /// The uses, sorted by path, then node or line.
    pub usages: Vec<LogicUsage>,
    /// Files the index holds after the search.
    pub files: usize,
    /// Files parsed by this search (new or changed since the last one).
    pub parsed: usize,
}

/// The project usage index and the last answer (an editor world resource; built on demand by
/// [`find`], never per frame).
#[derive(Resource, Default)]
pub struct LogicUsages {
    /// Each indexed file's content hash and extracted uses.
    files: BTreeMap<PathBuf, (u64, Arc<Vec<Fact>>)>,
    /// Files parsed since the index was created (a cached file is not counted again).
    pub parses: u64,
    /// The last answer (the Usages tab).
    pub last: Option<UsageAnswer>,
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Every key a condition tree reads.
fn condition_keys(condition: &Condition, out: &mut Vec<String>) {
    match condition {
        Condition::Always => {}
        Condition::Key(key) | Condition::Has(key) => out.push(key.clone()),
        Condition::Compare { key, .. } | Condition::Text { key, .. } => out.push(key.clone()),
        Condition::Not(inner) => condition_keys(inner, out),
        Condition::All(terms) | Condition::Any(terms) => {
            for t in terms {
                condition_keys(t, out);
            }
        }
    }
}

fn literal_string(binding: Option<&Binding>) -> Option<&str> {
    match binding {
        Some(Binding::Literal(Literal::String(s))) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// The uses one logic node makes: `(kind, name, how)` (names as written; keys not normalized).
fn node_uses(kind: &LogicNodeKind) -> Vec<(UsageKind, String, &'static str)> {
    let mut out = Vec::new();
    match kind {
        LogicNodeKind::Event { event, .. } => {
            if let Some(name) = event.inbox_name() {
                out.push((UsageKind::Event, name, "receives"));
            }
            if let LogicEvent::BlackboardChanged { key } = event {
                out.push((UsageKind::Key, key.clone(), "watches"));
            }
        }
        LogicNodeKind::Condition { condition } => {
            let mut keys = Vec::new();
            condition_keys(condition, &mut keys);
            out.extend(keys.into_iter().map(|k| (UsageKind::Key, k, "reads")));
        }
        LogicNodeKind::Action { task, set } => {
            match task.as_str() {
                "logic::EmitEvent" => {
                    if let Some(name) = literal_string(set.get("event")) {
                        out.push((UsageKind::Event, name.to_string(), "emits"));
                    }
                }
                "logic::SetBlackboard" => {
                    if let Some(key) = literal_string(set.get("key")) {
                        out.push((UsageKind::Key, key.to_string(), "writes"));
                    }
                }
                _ => {}
            }
            let registered = !BUILTIN_TASKS.contains(&task.as_str());
            for (param, binding) in set {
                if let Binding::BbRef(key) = binding {
                    out.push((UsageKind::Key, key.clone(), "reads"));
                }
                if registered {
                    // A registered task's parameters are written into the entity blackboard before
                    // each of its ticks (spec 102 § 3), and may name an animation parameter.
                    out.push((UsageKind::Key, param.clone(), "writes"));
                    out.push((UsageKind::Param, param.clone(), "sets"));
                    if let Some(name) = literal_string(Some(binding)) {
                        out.push((UsageKind::Param, name.to_string(), "sets"));
                    }
                }
            }
        }
    }
    out
}

/// The queries a node's "Find usages" offers, in the order the node names them (no duplicates).
#[must_use]
pub fn queries_for_node(doc: &LogicGraphDoc, node: NodeId) -> Vec<UsageQuery> {
    let Some(n) = doc.nodes.get(&node) else {
        return Vec::new();
    };
    let mut out: Vec<UsageQuery> = Vec::new();
    for (kind, name, _) in node_uses(&n.kind) {
        let query = UsageQuery::new(kind, name);
        let same = |q: &UsageQuery| {
            q.kind == query.kind
                && match q.kind {
                    UsageKind::Key => normalize_key(&q.name) == normalize_key(&query.name),
                    _ => q.name == query.name,
                }
        };
        if !out.iter().any(same) {
            out.push(query);
        }
    }
    out
}

fn logic_facts(text: &str) -> Vec<Fact> {
    let Ok(doc) = LogicGraphDoc::from_ron(text, &BTreeMap::new()) else {
        return Vec::new();
    };
    let owners = super::panel::node_owners(&doc);
    let mut out = Vec::new();
    for local in &doc.locals {
        out.push(Fact {
            kind: UsageKind::Key,
            name: format!("local/{}", local.name),
            how: "declares",
            node: None,
            rule: None,
            line: None,
            detail: "graph local".into(),
        });
    }
    for (id, node) in &doc.nodes {
        let detail = super::panel::node_title(&node.kind);
        for (kind, name, how) in node_uses(&node.kind) {
            out.push(Fact {
                kind,
                name: if kind == UsageKind::Key {
                    normalize_key(&name)
                } else {
                    name
                },
                how,
                node: Some(*id),
                rule: owners.get(id).map(|(rule, _)| rule.clone()),
                line: None,
                detail: detail.clone(),
            });
        }
    }
    out
}

/// The parameters an animation node tree reads, with where (`blend space`, `transition to Run`…).
fn anim_reads(node: &AnimNode, out: &mut Vec<(String, String)>) {
    let mut push = |name: &str, what: String| {
        if !name.is_empty() {
            out.push((name.to_string(), what));
        }
    };
    match node {
        AnimNode::BlendSpace1D(n) => push(&n.space.param, "blend space".into()),
        AnimNode::BlendSpace2D(n) => {
            push(&n.space.param_x, "blend space x".into());
            push(&n.space.param_y, "blend space y".into());
        }
        AnimNode::StateMachine(sm) => {
            for transition in &sm.transitions {
                for condition in &transition.conditions {
                    use regia_animation::state_machine::Condition as C;
                    if let C::Float { param, .. }
                    | C::Int { param, .. }
                    | C::Bool { param, .. }
                    | C::Trigger { param } = condition
                    {
                        push(param, format!("transition to {}", transition.to));
                    }
                }
            }
        }
        AnimNode::Blend(n) => push(&n.alpha_param, "blend alpha".into()),
        AnimNode::Additive(n) => push(&n.weight_param, "additive weight".into()),
        AnimNode::Layer(n) => {
            for layer in &n.layers {
                push(&layer.weight_param, "layer weight".into());
            }
        }
        AnimNode::TwoBoneIK(n) => {
            if let IkTarget::Param(p) = &n.target {
                push(p, "two-bone IK target".into());
            }
            if let Some(p) = &n.pole {
                push(p, "two-bone IK pole".into());
            }
            if let Some(p) = &n.weight_param {
                push(p, "two-bone IK weight".into());
            }
        }
        AnimNode::LookAt(n) => {
            if let IkTarget::Param(p) = &n.target {
                push(p, "look-at target".into());
            }
            if let Some(p) = &n.weight_param {
                push(p, "look-at weight".into());
            }
        }
        AnimNode::ChainIK(n) => {
            if let IkTarget::Param(p) = &n.target {
                push(p, "chain IK target".into());
            }
            if let Some(p) = &n.pole {
                push(p, "chain IK pole".into());
            }
            if let Some(p) = &n.weight_param {
                push(p, "chain IK weight".into());
            }
        }
        AnimNode::BodyAim(n) => {
            if let IkTarget::Param(p) = &n.target {
                push(p, "body aim target".into());
            }
            if let Some(p) = &n.weight_param {
                push(p, "body aim weight".into());
            }
        }
        AnimNode::StrideWarp(n) => push(&n.ground_speed_param, "stride warp ground speed".into()),
        AnimNode::OrientationWarp(n) => push(&n.heading_param, "orientation warp heading".into()),
        AnimNode::Clip(_) | AnimNode::RootMotion(_) | AnimNode::Slot(_) => {}
    }
    let children: Vec<&AnimNode> = match node {
        AnimNode::StateMachine(sm) => sm.states.iter().map(|(_, n)| n).collect(),
        AnimNode::Blend(n) => vec![&*n.a, &*n.b],
        AnimNode::Additive(n) => vec![&*n.base, &*n.additive],
        AnimNode::Layer(n) => std::iter::once(&*n.base)
            .chain(n.layers.iter().map(|l| &*l.graph))
            .collect(),
        AnimNode::TwoBoneIK(n) => vec![&*n.inner],
        AnimNode::LookAt(n) => vec![&*n.inner],
        AnimNode::ChainIK(n) => vec![&*n.inner],
        AnimNode::BodyAim(n) => vec![&*n.inner],
        AnimNode::StrideWarp(n) => vec![&*n.inner],
        AnimNode::OrientationWarp(n) => vec![&*n.inner],
        AnimNode::RootMotion(n) => vec![&*n.inner],
        AnimNode::Slot(n) => vec![&*n.source],
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => Vec::new(),
    };
    for child in children {
        anim_reads(child, out);
    }
}

fn anim_facts(text: &str) -> Vec<Fact> {
    let Ok(graph) = AnimGraph::from_ron_str(text) else {
        return Vec::new();
    };
    let fact = |name: &str, how, detail: String| Fact {
        kind: UsageKind::Param,
        name: name.to_string(),
        how,
        node: None,
        rule: None,
        line: None,
        detail,
    };
    let mut out: Vec<Fact> = graph
        .parameters
        .iter()
        .map(|p| fact(&p.0, "declares", "parameter".into()))
        .collect();
    let mut reads = Vec::new();
    anim_reads(&graph.root, &mut reads);
    out.extend(reads.into_iter().map(|(name, what)| fact(&name, "reads", what)));
    out
}

/// Code lines that set a parameter by a literal name (`set_float("Speed", …)`).
fn code_facts(text: &str) -> Vec<Fact> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for setter in CODE_SETTERS {
            let mut rest = line;
            while let Some(at) = rest.find(setter) {
                let after = &rest[at + setter.len()..];
                let Some(end) = after.find('"') else {
                    break;
                };
                let name = &after[..end];
                if !name.is_empty() {
                    out.push(Fact {
                        kind: UsageKind::Param,
                        name: name.to_string(),
                        how: "sets",
                        node: None,
                        rule: None,
                        line: Some(i + 1),
                        detail: line.trim().chars().take(80).collect(),
                    });
                }
                rest = &after[end..];
            }
        }
    }
    out
}

fn doc_of(path: &Path) -> &'static str {
    let name = path.to_string_lossy();
    if name.ends_with(".logic.ron") {
        "logic"
    } else if name.ends_with(".animgraph.ron") {
        "anim"
    } else {
        "code"
    }
}

impl LogicUsages {
    /// Brings the index up to date with `root` (new and changed files parsed, removed files
    /// dropped) and answers `query`. Unreadable files are skipped; a file that does not parse has no
    /// uses.
    pub fn search(&mut self, root: &Path, query: &UsageQuery) -> UsageAnswer {
        let _span = tracing::info_span!(target: "regia_editor", "logic_find_usages").entered();
        let files = crate::matgraph_editor::usages::project_files(root, &INDEXED_SUFFIXES);
        let mut parsed = 0;
        let mut fresh = BTreeMap::new();
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let hash = content_hash(&bytes);
            let facts = match self.files.get(&path) {
                Some((h, facts)) if *h == hash => facts.clone(),
                _ => {
                    parsed += 1;
                    let text = String::from_utf8_lossy(&bytes);
                    Arc::new(match doc_of(&path) {
                        "logic" => logic_facts(&text),
                        "anim" => anim_facts(&text),
                        _ => code_facts(&text),
                    })
                }
            };
            fresh.insert(path, (hash, facts));
        }
        self.files = fresh;
        self.parses += parsed as u64;
        let mut usages: Vec<LogicUsage> = self
            .files
            .iter()
            .flat_map(|(path, (_, facts))| {
                facts.iter().filter(|f| query.matches(f)).map(|f| LogicUsage {
                    path: path.clone(),
                    doc: doc_of(path),
                    how: f.how,
                    node: f.node,
                    rule: f.rule.clone(),
                    line: f.line,
                    detail: f.detail.clone(),
                })
            })
            .collect();
        usages.sort_by(|a, b| (&a.path, a.node, a.line, a.how).cmp(&(&b.path, b.node, b.line, b.how)));
        usages.dedup();
        UsageAnswer {
            query: query.clone(),
            usages,
            files: self.files.len(),
            parsed,
        }
    }
}

/// Runs `query` over the project (the agent root) and keeps the answer for the Usages tab, which
/// the Logic Graph panel then shows.
pub fn find(world: &mut World, query: &UsageQuery) -> UsageAnswer {
    let root = crate::agent_ext::agent_root(world);
    let mut index = world.remove_resource::<LogicUsages>().unwrap_or_default();
    let answer = index.search(&root, query);
    index.last = Some(answer.clone());
    world.insert_resource(index);
    let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
    editor.ui.show_tab(super::panel::BottomTab::Usages);
    editor.toast = format!(
        "{} use(s) of {} \"{}\"",
        answer.usages.len(),
        query.kind.name(),
        query.name
    );
    answer
}

/// Follows row `index` of the last answer: a logic graph opens in the Logic Graph panel with the
/// node selected and framed (the open graph is kept when it is the same file; another graph with
/// unsaved work is not replaced), an animation graph opens in its editor. Returns what was opened.
///
/// # Errors
///
/// `no_search` (no answer yet), `usage_missing` (no such row), `unsaved_changes`, `open_failed`,
/// `no_editor` (a code line: open it in a code editor), `unknown_node`.
pub fn open_usage(world: &mut World, index: usize) -> Result<Value, (&'static str, String)> {
    let usage = {
        let last = world
            .get_resource::<LogicUsages>()
            .and_then(|u| u.last.as_ref())
            .ok_or(("no_search", "no usage search has run yet".to_string()))?;
        last.usages.get(index).cloned().ok_or((
            "usage_missing",
            format!("the last answer has {} row(s); no row {index}", last.usages.len()),
        ))?
    };
    let shown = usage.path.display().to_string();
    match usage.doc {
        "logic" => {
            {
                let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
                let same = editor.path.as_deref().is_some_and(|p| same_file(p, &usage.path));
                if !same {
                    if editor.has_unsaved_work() {
                        return Err((
                            "unsaved_changes",
                            "the open logic graph has unsaved changes: save it first".into(),
                        ));
                    }
                    editor.open(&usage.path).map_err(|e| ("open_failed", e))?;
                }
            }
            match usage.node {
                Some(node) => super::problems::go_to_node(world, node)?,
                None => crate::editor_layout::LayoutRequests::push(
                    world,
                    crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::LogicGraph),
                ),
            }
            Ok(json!({ "path": shown, "doc": "logic", "node": usage.node }))
        }
        "anim" => {
            let rel = crate::asset_open::project_relative(&shown);
            crate::asset_open::open(world, &rel).map_err(|e| ("open_failed", e.message))?;
            Ok(json!({ "path": shown, "doc": "anim", "node": Value::Null }))
        }
        _ => Err((
            "no_editor",
            format!(
                "{shown}:{} is code: open it in a code editor",
                usage.line.unwrap_or(0)
            ),
        )),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// The inspector's "Find usages" buttons for the selected node (the keyboard path of the node's
/// context menu: each is a focusable button).
pub fn inspector_row(
    ui: &mut egui::Ui,
    doc: &LogicGraphDoc,
    node: NodeId,
    edits: &mut Vec<super::panel::PanelEdit>,
) {
    let queries = queries_for_node(doc, node);
    if queries.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for query in queries {
            if ui.button(query.label()).clicked() {
                edits.push(super::panel::PanelEdit::FindUsages(query));
            }
        }
    });
    ui.separator();
}

/// The Usages tab: the last answer, one focusable row per use (Enter or a click opens it).
pub fn usages_tab(
    ui: &mut egui::Ui,
    answer: Option<&UsageAnswer>,
    root: &Path,
    edits: &mut Vec<super::panel::PanelEdit>,
) {
    let Some(answer) = answer else {
        ui.weak("Right-click a node (or use the inspector) and pick Find usages.");
        return;
    };
    ui.strong(format!(
        "{} use(s) of {} \"{}\"",
        answer.usages.len(),
        answer.query.kind.name(),
        answer.query.name
    ));
    if answer.usages.is_empty() {
        ui.weak("No file in the project uses it.");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("logic-usages")
        .show(ui, |ui| {
            for (i, usage) in answer.usages.iter().enumerate() {
                let text = usage.row_text(root);
                let response = ui.add(egui::Button::new(text).frame(false));
                let response = if usage.doc == "code" {
                    response.on_hover_text("Code: open it in a code editor")
                } else {
                    response.on_hover_text("Open it here")
                };
                if response.clicked() && usage.doc != "code" {
                    edits.push(super::panel::PanelEdit::OpenUsage(i));
                }
            }
        });
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// `F0002` with `data.reason`, as the other logic methods answer.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

const PARAMS: [ParamSchema; 4] = [
    ParamSchema {
        name: "event",
        kind: "string",
        required: false,
        description: "An event name (as the inbox receives it): graphs that emit and receive it.",
    },
    ParamSchema {
        name: "key",
        kind: "string",
        required: false,
        description: "A blackboard key (`x` and `entity/x` match each other; `local/x`, `world/<board>/x`): graphs that read, write, watch or declare it.",
    },
    ParamSchema {
        name: "param",
        kind: "string",
        required: false,
        description: "An animation-graph parameter: animation graphs that declare or read it, logic actions and code lines that set it.",
    },
    ParamSchema {
        name: "open",
        kind: "number",
        required: false,
        description: "Follow this row (`index`) of the answer, as a click on it in the Usages tab: a logic graph opens at the node, an animation graph in its editor.",
    },
];

/// `editor.logic_find_usages`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.logic_find_usages",
            tool: "logic_find_usages",
            description: "Find usages across the project (exactly one of event, key, param): which logic graphs \
                          emit or receive an event, read, write or watch a blackboard key, and where an \
                          animation parameter is declared, read or set (animation graphs, logic actions, code). \
                          What a logic node's Find usages menu item does; the answer fills the Logic Graph \
                          panel's Usages tab. The index is cached by file content hash.",
            params: &PARAMS,
            result: "{ kind, name, usages: [{ index, path, doc, how, node, rule, line, detail }], files, parsed, opened? }",
            mutating: false,
            permission: Permission::Read,
        },
        find_usages_method as Handler,
    )]
}

fn find_usages_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut given = Vec::new();
    for kind in UsageKind::ALL {
        match params.get(kind.name()) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if !s.trim().is_empty() => given.push(UsageQuery::new(kind, s.trim())),
            Some(_) => {
                return Err(refused(
                    "bad_value",
                    format!("`{}` must be a non-empty string", kind.name()),
                    "pass a name, e.g. { \"event\": \"door_opened\" }",
                ));
            }
        }
    }
    let query = match given.len() {
        1 => given.remove(0),
        0 => {
            return Err(refused(
                "missing_param",
                "one of `event`, `key` or `param` is required",
                "pass exactly one, e.g. { \"key\": \"health\" }",
            ));
        }
        _ => {
            return Err(refused(
                "bad_value",
                "pass exactly one of `event`, `key` or `param`",
                "search one name at a time",
            ));
        }
    };
    let open = match params.get("open") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| {
            refused(
                "bad_value",
                "`open` must be a row index",
                "take `index` from usages of this answer",
            )
        })?),
    };
    let answer = find(world, &query);
    let mut out = json!({
        "kind": query.kind.name(),
        "name": query.name,
        "usages": answer.usages.iter().enumerate().map(|(i, u)| u.to_json(i)).collect::<Vec<_>>(),
        "files": answer.files,
        "parsed": answer.parsed,
    });
    if let Some(index) = open {
        let opened = open_usage(world, index).map_err(|(code, message)| {
            refused(
                code,
                message,
                "take `index` from usages of this answer; save the open graph before opening another",
            )
        })?;
        out["opened"] = opened;
    }
    Ok(out)
}
