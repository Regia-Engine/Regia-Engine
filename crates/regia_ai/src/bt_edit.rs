// SPDX-License-Identifier: MPL-2.0
//! Editing and checking `.bt.ron` behavior trees without a running world (`docs/specs/22-tier2.md`
//! § T2.2, ADR-2408): the text round trip (`parse_bt_ron`, `to_canonical_ron`), a validator that
//! reports every problem with a stable code ([`validate`]), and a small set of structural edit
//! operations ([`BtOp`], [`apply_ops`]) that keep the flat node arena consistent when nodes are
//! added, removed, connected or reordered.
//!
//! The editor panel and the `ai.bt_*` agent methods both use this module, so a tree edited in the
//! panel and one edited over the protocol go through the same code. The `.bt.ron` shape is
//! unchanged: this module only reads and writes the existing [`BehaviorTreeDef`].

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::bt_value::{AbortMode, BtCompareOp, BtLiteral, BtValueKind};
use crate::tree::{
    BT_FORMAT_VERSION, BehaviorTreeDef, BtTaskRegistry, DefKind, DefNode, ParallelPolicy, node_uses_v2,
    static_issues,
};

/// How bad an [`BtIssue`] is. Errors stop a tree from compiling; warnings do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BtSeverity {
    /// The tree would not compile.
    Error,
    /// The tree compiles but something is probably a mistake.
    Warning,
}

/// One problem found in a tree, with a stable lowercase code.
///
/// Codes: `empty_tree`, `root_out_of_range`, `child_out_of_range`, `condition_has_children`,
/// `leaf_has_children`, `decorator_arity`, `bad_seconds`, `empty_key`, `empty_task`,
/// `unknown_task`, `shared_node`, `cycle`, `type_mismatch`, `bad_compare`, `compare_has_children`,
/// `abort_not_allowed`, `abort_without_key`, `empty_state_tree`, `bad_state_tree`,
/// `state_tree_has_children` and `too_deep` (errors); `empty_composite`, `unreachable_node` and
/// `duplicate_name` (warnings).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BtIssue {
    /// The stable code.
    pub code: &'static str,
    /// Error or warning.
    pub severity: BtSeverity,
    /// The node index the problem belongs to, when it belongs to one.
    pub node: Option<usize>,
    /// A sentence for people.
    pub message: String,
}

impl BtIssue {
    fn error(code: &'static str, node: Option<usize>, message: String) -> Self {
        Self {
            code,
            severity: BtSeverity::Error,
            node,
            message,
        }
    }

    fn warning(code: &'static str, node: Option<usize>, message: String) -> Self {
        Self {
            code,
            severity: BtSeverity::Warning,
            node,
            message,
        }
    }
}

/// Whether `issues` holds no error (warnings are allowed).
#[must_use]
pub fn is_valid(issues: &[BtIssue]) -> bool {
    issues.iter().all(|i| i.severity != BtSeverity::Error)
}

/// Whether a kind runs exactly one child.
#[must_use]
pub fn is_decorator(kind: DefKind) -> bool {
    matches!(
        kind,
        DefKind::Inverter | DefKind::Cooldown | DefKind::Repeat | DefKind::Timeout | DefKind::Service
    )
}

/// Whether a kind takes no children.
#[must_use]
pub fn is_childless(kind: DefKind) -> bool {
    matches!(
        kind,
        DefKind::Leaf | DefKind::Condition | DefKind::Compare | DefKind::StateTree
    )
}

/// Checks `def` and lists every problem. With a `registry`, leaf and service task paths are
/// checked against it (`unknown_task`); without one that check is skipped. Never panics on a
/// malformed definition.
#[must_use]
pub fn validate(def: &BehaviorTreeDef, registry: Option<&BtTaskRegistry>) -> Vec<BtIssue> {
    let _span = tracing::info_span!(target: "regia_ai", "bt_validate").entered();
    let count = def.nodes.len();
    let mut issues = Vec::new();
    if count == 0 {
        issues.push(BtIssue::error(
            "empty_tree",
            None,
            "the tree has no nodes; add a root".into(),
        ));
        return issues;
    }
    if def.root >= count {
        issues.push(BtIssue::error(
            "root_out_of_range",
            None,
            format!("root index {} is outside the {count}-node arena", def.root),
        ));
    }
    for (index, node) in def.nodes.iter().enumerate() {
        node_issues(index, node, count, registry, &mut issues);
    }
    graph_issues(def, &mut issues);
    let mut seen_names = BTreeSet::new();
    for (index, node) in def.nodes.iter().enumerate() {
        if !node.name.is_empty() && !seen_names.insert(node.name.as_str()) {
            issues.push(BtIssue::warning(
                "duplicate_name",
                Some(index),
                format!("node {index} reuses the name {:?}", node.name),
            ));
        }
    }
    issues
}

fn node_issues(
    index: usize,
    node: &DefNode,
    count: usize,
    registry: Option<&BtTaskRegistry>,
    issues: &mut Vec<BtIssue>,
) {
    for child in &node.children {
        if *child >= count {
            issues.push(BtIssue::error(
                "child_out_of_range",
                Some(index),
                format!(
                    "node {index} ({:?}) has child index {child} outside the {count}-node arena",
                    node.name
                ),
            ));
        }
    }
    match node.kind {
        DefKind::Condition => {
            if !node.children.is_empty() {
                issues.push(BtIssue::error(
                    "condition_has_children",
                    Some(index),
                    format!("condition {index} ({:?}) takes no children", node.name),
                ));
            }
            // `empty_key` comes from `static_issues`, shared with the runtime compiler.
        }
        DefKind::Leaf if !node.children.is_empty() => {
            issues.push(BtIssue::error(
                "leaf_has_children",
                Some(index),
                format!("leaf {index} ({:?}) takes no children", node.name),
            ));
        }
        DefKind::Selector | DefKind::Sequence | DefKind::Parallel if node.children.is_empty() => {
            issues.push(BtIssue::warning(
                "empty_composite",
                Some(index),
                format!("{:?} {index} ({:?}) has no children", node.kind, node.name),
            ));
        }
        _ => {}
    }
    if is_decorator(node.kind) && node.children.len() != 1 {
        issues.push(BtIssue::error(
            "decorator_arity",
            Some(index),
            format!(
                "{:?} {index} ({:?}) needs exactly one child, has {}",
                node.kind,
                node.name,
                node.children.len()
            ),
        ));
    }
    if matches!(node.kind, DefKind::Cooldown | DefKind::Timeout)
        && !(node.seconds.is_finite() && node.seconds >= 0.0)
    {
        issues.push(BtIssue::error(
            "bad_seconds",
            Some(index),
            format!(
                "{:?} {index} ({:?}) needs a finite, non-negative `seconds` (got {})",
                node.kind, node.name, node.seconds
            ),
        ));
    }
    for (code, message) in static_issues(node) {
        issues.push(BtIssue::error(code, Some(index), message));
    }
    if matches!(node.kind, DefKind::Leaf | DefKind::Service) {
        if node.task.is_empty() {
            issues.push(BtIssue::error(
                "empty_task",
                Some(index),
                format!("{:?} {index} ({:?}) needs a task type path", node.kind, node.name),
            ));
        } else if let Some(registry) = registry
            && registry.construct(&node.task).is_none()
        {
            issues.push(BtIssue::error(
                "unknown_task",
                Some(index),
                format!(
                    "{:?} {index} ({:?}) references unregistered task type {:?}",
                    node.kind, node.name, node.task
                ),
            ));
        }
    }
}

/// Shared nodes, cycles and unreachable nodes. Skips out-of-range indices (reported elsewhere).
fn graph_issues(def: &BehaviorTreeDef, issues: &mut Vec<BtIssue>) {
    let count = def.nodes.len();
    let mut reached = vec![false; count];
    if def.root < count {
        let mut stack = vec![(def.root, 1usize)];
        let mut too_deep = false;
        while let Some((index, depth)) = stack.pop() {
            if std::mem::replace(&mut reached[index], true) {
                issues.push(BtIssue::error(
                    "shared_node",
                    Some(index),
                    format!("node {index} is reachable twice (a node with two parents, or a cycle)"),
                ));
                continue;
            }
            if depth > crate::tree::MAX_BT_DEPTH && !too_deep {
                // Reported once, at the first node past the cap.
                too_deep = true;
                issues.push(BtIssue::error(
                    "too_deep",
                    Some(index),
                    format!(
                        "node {index} is {depth} levels deep; a tree may be at most {} deep",
                        crate::tree::MAX_BT_DEPTH
                    ),
                ));
            }
            stack.extend(
                def.nodes[index]
                    .children
                    .iter()
                    .filter(|c| **c < count)
                    .map(|child| (*child, depth + 1)),
            );
        }
    }
    // Cycles anywhere in the arena (unreachable nodes are compiled too): three-color DFS.
    let mut color = vec![0u8; count];
    for start in 0..count {
        if color[start] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = 1;
        while let Some(&(node, next)) = stack.last() {
            if let Some(&child) = def.nodes[node].children.get(next) {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if child >= count {
                    continue;
                }
                match color[child] {
                    1 => issues.push(BtIssue::error(
                        "cycle",
                        Some(node),
                        format!(
                            "node {node} ({:?}) is part of a cycle through child {child}",
                            def.nodes[node].name
                        ),
                    )),
                    0 => {
                        color[child] = 1;
                        stack.push((child, 0));
                    }
                    _ => {}
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    if def.root < count {
        for (index, seen) in reached.iter().enumerate() {
            if !seen {
                issues.push(BtIssue::warning(
                    "unreachable_node",
                    Some(index),
                    format!(
                        "node {index} ({:?}) is not reachable from the root",
                        def.nodes[index].name
                    ),
                ));
            }
        }
    }
}

/// Parses `.bt.ron` text.
///
/// # Errors
///
/// The text is not a valid `.bt.ron` (RON syntax or shape).
pub fn parse_bt_ron(text: &str) -> Result<BehaviorTreeDef, String> {
    let mut def: BehaviorTreeDef = ron::from_str(text).map_err(|e| e.to_string())?;
    if def.format_version == 0 {
        def.format_version = 1;
    }
    if def.format_version > BT_FORMAT_VERSION {
        return Err(format!(
            ".bt.ron format_version {} is newer than {BT_FORMAT_VERSION}",
            def.format_version
        ));
    }
    if def.nodes.iter().any(node_uses_v2) {
        def.format_version = def.format_version.max(2);
    }
    Ok(def)
}

/// Rewrites a v1 `.bt.ron` as v2 and keeps every other byte.
///
/// A file with no `format_version` gains one. A file that already says `format_version: 1` has
/// that number replaced. A file already at v2 is returned unchanged. A newer version is refused.
///
/// # Errors
///
/// The text is not a `.bt.ron`, or its version is newer than [`BT_FORMAT_VERSION`].
pub fn migrate_bt_ron(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("behavior tree RON is empty".to_string());
    }
    #[derive(serde::Deserialize)]
    #[serde(rename = "BehaviorTreeDef")]
    struct Header<'a> {
        #[serde(default, borrow, deserialize_with = "borrow_version")]
        format_version: Option<&'a ron::value::RawValue>,
    }
    let header: Header<'_> = ron::from_str(text).map_err(|e| e.to_string())?;
    let version = header
        .format_version
        .map(|raw| raw.into_rust::<u32>())
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(1);
    if version > BT_FORMAT_VERSION {
        return Err(format!(
            ".bt.ron format_version {version} is newer than {BT_FORMAT_VERSION}"
        ));
    }
    if version >= 2 {
        return Ok(text.to_string());
    }
    if let Some(raw) = header.format_version {
        // The borrowed raw value points into `text`. Trim its surrounding comments and spaces,
        // then replace the whole integer token (RON also accepts hex and digit separators).
        let token = raw.trim().get_ron();
        let start = token
            .as_ptr()
            .addr()
            .checked_sub(text.as_ptr().addr())
            .ok_or_else(|| "version value is outside the RON input".to_string())?;
        let end = start
            .checked_add(token.len())
            .filter(|end| *end <= text.len())
            .ok_or_else(|| "version value is outside the RON input".to_string())?;
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..start]);
        out.push_str(&BT_FORMAT_VERSION.to_string());
        out.push_str(&text[end..]);
        return Ok(out);
    }
    insert_format_version(text)
}

fn borrow_version<'de, D>(deserializer: D) -> Result<Option<&'de ron::value::RawValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <&'de ron::value::RawValue as serde::Deserialize<'de>>::deserialize(deserializer).map(Some)
}

fn insert_format_version(text: &str) -> Result<String, String> {
    let open = root_paren(text).ok_or_else(|| "behavior tree RON has no root struct".to_string())?;
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 24);
    out.push_str(&text[..=open]);
    let rest = &bytes[open + 1..];
    if rest.first() == Some(&b'\n') {
        out.push('\n');
        out.push_str("    format_version: 2,\n");
        out.push_str(&text[open + 2..]);
    } else if rest.first() == Some(&b'\r') && rest.get(1) == Some(&b'\n') {
        out.push_str("\r\n    format_version: 2,\r\n");
        out.push_str(&text[open + 3..]);
    } else {
        out.push_str("format_version: 2, ");
        out.push_str(&text[open + 1..]);
    }
    Ok(out)
}

/// The byte index of the root `(`, skipping document attributes and comments.
fn root_paren(text: &str) -> Option<usize> {
    let parser = ron::de::Deserializer::from_str(text).ok()?;
    let remaining = parser.remainder();
    let offset = text.len() - remaining.len();
    let bytes = remaining.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => return Some(offset + i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => i += 1,
        }
    }
    None
}

/// Writes `def` in the canonical `.bt.ron` layout: `root`, then one line per node with every field
/// in declaration order. The same definition always gives the same text, and
/// [`parse_bt_ron`] reads it back to an equal definition.
///
/// # Errors
///
/// A `seconds` value is not finite (RON cannot hold it).
pub fn to_canonical_ron(def: &BehaviorTreeDef) -> Result<String, String> {
    let mut out = String::from("(\n");
    if def.format_version >= 2 {
        let _ = writeln!(out, "    format_version: {},", def.format_version);
    }
    let _ = writeln!(out, "    root: {},\n    nodes: [", def.root);
    for (index, node) in def.nodes.iter().enumerate() {
        if !node.seconds.is_finite() {
            return Err(format!("node {index} has a non-finite `seconds`"));
        }
        let kind = ron::ser::to_string(&node.kind).map_err(|e| e.to_string())?;
        let policy = ron::ser::to_string(&node.policy).map_err(|e| e.to_string())?;
        let name = ron::ser::to_string(&node.name).map_err(|e| e.to_string())?;
        let key = ron::ser::to_string(&node.key).map_err(|e| e.to_string())?;
        let task = ron::ser::to_string(&node.task).map_err(|e| e.to_string())?;
        let children = node
            .children
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let times = match node.times {
            Some(n) => format!("Some({n})"),
            None => "None".to_string(),
        };
        let mut line = format!(
            "        (name: {name}, kind: {kind}, children: [{children}], key: {key}, seconds: {:?}, times: {times}, task: {task}, policy: {policy}",
            node.seconds
        );
        if node.abort != AbortMode::None {
            let value = ron::ser::to_string(&node.abort).map_err(|e| e.to_string())?;
            let _ = write!(line, ", abort: {value}");
        }
        if node.op != BtCompareOp::Eq {
            let value = ron::ser::to_string(&node.op).map_err(|e| e.to_string())?;
            let _ = write!(line, ", op: {value}");
        }
        if node.value_kind != BtValueKind::Bool {
            let value = ron::ser::to_string(&node.value_kind).map_err(|e| e.to_string())?;
            let _ = write!(line, ", value_kind: {value}");
        }
        if node.literal.is_some() {
            let value = ron::ser::to_string(&node.literal).map_err(|e| e.to_string())?;
            let _ = write!(line, ", literal: {value}");
        }
        if node.literal_hi.is_some() {
            let value = ron::ser::to_string(&node.literal_hi).map_err(|e| e.to_string())?;
            let _ = write!(line, ", literal_hi: {value}");
        }
        if !node.rhs_key.is_empty() {
            let value = ron::ser::to_string(&node.rhs_key).map_err(|e| e.to_string())?;
            let _ = write!(line, ", rhs_key: {value}");
        }
        line.push_str("),");
        let _ = writeln!(out, "{line}");
    }
    out.push_str("    ],\n)\n");
    Ok(out)
}

/// A failed edit: a stable lowercase code plus a sentence.
///
/// Codes: `bad_node`, `bad_root`, `no_children_allowed`, `decorator_full`, `would_cycle`,
/// `not_a_child`, `bad_index`, `bad_field`, `too_many_nodes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtEditError {
    /// The stable code.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl BtEditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BtEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BtEditError {}

/// The most nodes an edit may grow a tree to.
pub const MAX_BT_NODES: usize = 4096;

/// One structural edit of a tree. Reads from JSON as `{ "op": "add_node", "kind": "selector", ... }`
/// (`snake_case` op names).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BtOp {
    /// Adds a node. With a `parent` it is connected there (at `index`, default last); with no
    /// parent and an empty tree it becomes the root; otherwise it is left unconnected.
    AddNode {
        /// What the node does.
        kind: DefKind,
        /// Debug name (default: the kind and the node's index).
        #[serde(default)]
        name: Option<String>,
        /// Attach under this node.
        #[serde(default)]
        parent: Option<usize>,
        /// Position among the parent's children.
        #[serde(default)]
        index: Option<usize>,
        /// Task type path (leaf, service).
        #[serde(default)]
        task: Option<String>,
        /// Blackboard key (condition).
        #[serde(default)]
        key: Option<String>,
        /// Interval (cooldown, timeout).
        #[serde(default)]
        seconds: Option<f32>,
        /// Success cap (repeat).
        #[serde(default)]
        times: Option<u32>,
        /// Parallel policy.
        #[serde(default)]
        policy: Option<ParallelPolicy>,
        /// Abort mode (condition, compare, decorator).
        #[serde(default)]
        abort: Option<AbortMode>,
        /// Comparison. Named `compare` in JSON because `op` is the edit tag.
        #[serde(default, rename = "compare")]
        op: Option<BtCompareOp>,
        /// Value type (compare).
        #[serde(default)]
        value_kind: Option<BtValueKind>,
        /// Constant or low bound (compare).
        #[serde(default)]
        literal: Option<BtLiteral>,
        /// High bound (compare).
        #[serde(default)]
        literal_hi: Option<BtLiteral>,
        /// Second blackboard key (compare).
        #[serde(default)]
        rhs_key: Option<String>,
    },
    /// Removes a node and renumbers the arena. With `subtree` its descendants go too; otherwise
    /// its children stay as unconnected nodes. If it was the root, the root becomes node 0.
    RemoveNode {
        /// The node.
        node: usize,
        /// Remove the descendants as well.
        #[serde(default)]
        subtree: bool,
    },
    /// Connects `child` under `parent` (moving it when it already has a parent).
    Connect {
        /// The parent.
        parent: usize,
        /// The child.
        child: usize,
        /// Position among the parent's children (default last).
        #[serde(default)]
        index: Option<usize>,
    },
    /// Cuts the edge from `parent` to `child`; the child stays as an unconnected node.
    Disconnect {
        /// The parent.
        parent: usize,
        /// The child.
        child: usize,
    },
    /// Moves a child within its parent's list.
    Reorder {
        /// The parent.
        parent: usize,
        /// The child's current position.
        from: usize,
        /// The new position.
        to: usize,
    },
    /// Changes the fields of a node; absent fields stay.
    SetNode {
        /// The node.
        node: usize,
        /// New name.
        #[serde(default)]
        name: Option<String>,
        /// New kind.
        #[serde(default)]
        kind: Option<DefKind>,
        /// New blackboard key.
        #[serde(default)]
        key: Option<String>,
        /// New interval.
        #[serde(default)]
        seconds: Option<f32>,
        /// New success cap.
        #[serde(default)]
        times: Option<u32>,
        /// Clear the success cap (repeat for ever).
        #[serde(default)]
        clear_times: bool,
        /// New task path.
        #[serde(default)]
        task: Option<String>,
        /// New parallel policy.
        #[serde(default)]
        policy: Option<ParallelPolicy>,
        /// New abort mode.
        #[serde(default)]
        abort: Option<AbortMode>,
        /// New comparison. Named `compare` in JSON because `op` is the edit tag.
        #[serde(default, rename = "compare")]
        op: Option<BtCompareOp>,
        /// New value type.
        #[serde(default)]
        value_kind: Option<BtValueKind>,
        /// New constant or low bound.
        #[serde(default)]
        literal: Option<BtLiteral>,
        /// New high bound.
        #[serde(default)]
        literal_hi: Option<BtLiteral>,
        /// Clear the constant.
        #[serde(default)]
        clear_literal: bool,
        /// Clear the high bound.
        #[serde(default)]
        clear_literal_hi: bool,
        /// New second key.
        #[serde(default)]
        rhs_key: Option<String>,
    },
    /// Makes `node` the root.
    SetRoot {
        /// The node.
        node: usize,
    },
}

fn check_node(def: &BehaviorTreeDef, node: usize) -> Result<(), BtEditError> {
    if node < def.nodes.len() {
        Ok(())
    } else {
        Err(BtEditError::new(
            "bad_node",
            format!("node {node} is outside the {}-node arena", def.nodes.len()),
        ))
    }
}

fn is_descendant(def: &BehaviorTreeDef, ancestor: usize, node: usize) -> bool {
    let mut stack = vec![ancestor];
    let mut seen = BTreeSet::new();
    while let Some(i) = stack.pop() {
        if i == node {
            return true;
        }
        if seen.insert(i) {
            stack.extend(
                def.nodes[i]
                    .children
                    .iter()
                    .copied()
                    .filter(|c| *c < def.nodes.len()),
            );
        }
    }
    false
}

fn check_room(def: &BehaviorTreeDef, parent: usize, adding: usize) -> Result<(), BtEditError> {
    let node = &def.nodes[parent];
    if is_childless(node.kind) {
        return Err(BtEditError::new(
            "no_children_allowed",
            format!("{:?} node {parent} takes no children", node.kind),
        ));
    }
    if is_decorator(node.kind) && node.children.len() + adding > 1 {
        return Err(BtEditError::new(
            "decorator_full",
            format!("{:?} node {parent} already has its one child", node.kind),
        ));
    }
    Ok(())
}

fn insert_child(
    def: &mut BehaviorTreeDef,
    parent: usize,
    child: usize,
    index: Option<usize>,
) -> Result<(), BtEditError> {
    let len = def.nodes[parent].children.len();
    let at = index.unwrap_or(len);
    if at > len {
        return Err(BtEditError::new(
            "bad_index",
            format!("index {at} is past the {len} children"),
        ));
    }
    def.nodes[parent].children.insert(at, child);
    Ok(())
}

/// Removes every edge to `child`.
fn detach(def: &mut BehaviorTreeDef, child: usize) {
    for n in &mut def.nodes {
        n.children.retain(|c| *c != child);
    }
}

fn remove_nodes(def: &mut BehaviorTreeDef, doomed: &BTreeSet<usize>) {
    let old_root = def.root;
    let mut map = vec![usize::MAX; def.nodes.len()];
    let mut next = 0;
    for (i, slot) in map.iter_mut().enumerate() {
        if !doomed.contains(&i) {
            *slot = next;
            next += 1;
        }
    }
    let mut index = 0;
    def.nodes.retain(|_| {
        let keep = !doomed.contains(&index);
        index += 1;
        keep
    });
    for n in &mut def.nodes {
        n.children = n
            .children
            .iter()
            .filter_map(|c| map.get(*c).copied().filter(|m| *m != usize::MAX))
            .collect();
    }
    def.root = match map.get(old_root) {
        Some(m) if *m != usize::MAX => *m,
        _ => 0,
    };
}

fn default_name(kind: DefKind, index: usize) -> String {
    let kind = ron::ser::to_string(&kind).unwrap_or_default();
    format!("{kind}_{index}")
}

/// Applies one operation. Returns the index of the node it created, if any.
///
/// # Errors
///
/// The operation does not fit the tree; the tree is left as it was.
pub fn apply_op(def: &mut BehaviorTreeDef, op: &BtOp) -> Result<Option<usize>, BtEditError> {
    let backup = def.clone();
    let result = apply_op_inner(def, op);
    if result.is_err() {
        *def = backup;
    } else if def.nodes.iter().any(node_uses_v2) {
        def.format_version = def.format_version.max(2);
    }
    result
}

fn apply_op_inner(def: &mut BehaviorTreeDef, op: &BtOp) -> Result<Option<usize>, BtEditError> {
    match op {
        BtOp::AddNode {
            kind,
            name,
            parent,
            index,
            task,
            key,
            seconds,
            times,
            policy,
            abort,
            op,
            value_kind,
            literal,
            literal_hi,
            rhs_key,
        } => {
            if def.nodes.len() >= MAX_BT_NODES {
                return Err(BtEditError::new(
                    "too_many_nodes",
                    format!("a tree holds at most {MAX_BT_NODES} nodes"),
                ));
            }
            if let Some(p) = parent {
                check_node(def, *p)?;
                check_room(def, *p, 1)?;
            }
            let new = def.nodes.len();
            def.nodes.push(DefNode {
                name: name.clone().unwrap_or_else(|| default_name(*kind, new)),
                kind: *kind,
                task: task.clone().unwrap_or_default(),
                key: key.clone().unwrap_or_default(),
                seconds: seconds.unwrap_or(0.0),
                times: *times,
                policy: policy.unwrap_or_default(),
                children: Vec::new(),
                abort: abort.unwrap_or(AbortMode::None),
                op: op.unwrap_or(BtCompareOp::Eq),
                value_kind: value_kind.unwrap_or(BtValueKind::Bool),
                literal: literal.clone(),
                literal_hi: literal_hi.clone(),
                rhs_key: rhs_key.clone().unwrap_or_default(),
            });
            match parent {
                Some(p) => insert_child(def, *p, new, *index)?,
                None if new == 0 => def.root = 0,
                None => {}
            }
            Ok(Some(new))
        }
        BtOp::RemoveNode { node, subtree } => {
            check_node(def, *node)?;
            let mut doomed = BTreeSet::from([*node]);
            if *subtree {
                let mut stack = vec![*node];
                while let Some(i) = stack.pop() {
                    for c in def.nodes[i].children.clone() {
                        if c < def.nodes.len() && doomed.insert(c) {
                            stack.push(c);
                        }
                    }
                }
            }
            remove_nodes(def, &doomed);
            Ok(None)
        }
        BtOp::Connect { parent, child, index } => {
            check_node(def, *parent)?;
            check_node(def, *child)?;
            if parent == child || is_descendant(def, *child, *parent) {
                return Err(BtEditError::new(
                    "would_cycle",
                    format!("connecting {child} under {parent} would make a cycle"),
                ));
            }
            detach(def, *child);
            check_room(def, *parent, 1)?;
            insert_child(def, *parent, *child, *index)?;
            Ok(None)
        }
        BtOp::Disconnect { parent, child } => {
            check_node(def, *parent)?;
            check_node(def, *child)?;
            let before = def.nodes[*parent].children.len();
            def.nodes[*parent].children.retain(|c| c != child);
            if def.nodes[*parent].children.len() == before {
                return Err(BtEditError::new(
                    "not_a_child",
                    format!("{child} is not a child of {parent}"),
                ));
            }
            Ok(None)
        }
        BtOp::Reorder { parent, from, to } => {
            check_node(def, *parent)?;
            let len = def.nodes[*parent].children.len();
            if *from >= len || *to >= len {
                return Err(BtEditError::new(
                    "bad_index",
                    format!("positions {from} and {to} must be below the {len} children"),
                ));
            }
            let child = def.nodes[*parent].children.remove(*from);
            def.nodes[*parent].children.insert(*to, child);
            Ok(None)
        }
        BtOp::SetNode {
            node,
            name,
            kind,
            key,
            seconds,
            times,
            clear_times,
            task,
            policy,
            abort,
            op,
            value_kind,
            literal,
            literal_hi,
            clear_literal,
            clear_literal_hi,
            rhs_key,
        } => {
            check_node(def, *node)?;
            if let Some(s) = seconds
                && !s.is_finite()
            {
                return Err(BtEditError::new("bad_field", "`seconds` must be finite"));
            }
            let n = &mut def.nodes[*node];
            if let Some(v) = name {
                n.name.clone_from(v);
            }
            if let Some(v) = kind {
                n.kind = *v;
            }
            if let Some(v) = key {
                n.key.clone_from(v);
            }
            if let Some(v) = seconds {
                n.seconds = *v;
            }
            if *clear_times {
                n.times = None;
            }
            if let Some(v) = times {
                n.times = Some(*v);
            }
            if let Some(v) = task {
                n.task.clone_from(v);
            }
            if let Some(v) = policy {
                n.policy = *v;
            }
            if let Some(v) = abort {
                n.abort = *v;
            }
            if let Some(v) = op {
                n.op = *v;
            }
            if let Some(v) = value_kind {
                n.value_kind = *v;
            }
            if *clear_literal {
                n.literal = None;
            }
            if let Some(v) = literal {
                n.literal = Some(v.clone());
            }
            if *clear_literal_hi {
                n.literal_hi = None;
            }
            if let Some(v) = literal_hi {
                n.literal_hi = Some(v.clone());
            }
            if let Some(v) = rhs_key {
                n.rhs_key.clone_from(v);
            }
            Ok(None)
        }
        BtOp::SetRoot { node } => {
            if *node >= def.nodes.len() {
                return Err(BtEditError::new(
                    "bad_root",
                    format!("node {node} does not exist"),
                ));
            }
            def.root = *node;
            Ok(None)
        }
    }
}

/// A failed batch: which operation failed and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtBatchError {
    /// The position of the failing operation in the list.
    pub op_index: usize,
    /// Why it failed.
    pub error: BtEditError,
}

/// Applies `ops` in order, all or nothing: the first failure leaves `def` untouched. Returns, per
/// operation, the index of the node it created.
///
/// # Errors
///
/// The first operation that does not fit the tree.
pub fn apply_ops(def: &mut BehaviorTreeDef, ops: &[BtOp]) -> Result<Vec<Option<usize>>, BtBatchError> {
    let mut work = def.clone();
    let mut created = Vec::with_capacity(ops.len());
    for (op_index, op) in ops.iter().enumerate() {
        created.push(apply_op(&mut work, op).map_err(|error| BtBatchError { op_index, error })?);
    }
    *def = work;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(kind: DefKind, parent: Option<usize>) -> BtOp {
        BtOp::AddNode {
            kind,
            name: None,
            parent,
            index: None,
            task: None,
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

    fn leaf(parent: usize, task: &str) -> BtOp {
        BtOp::AddNode {
            kind: DefKind::Leaf,
            name: None,
            parent: Some(parent),
            index: None,
            task: Some(task.into()),
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

    fn codes(issues: &[BtIssue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn selector_with_two_leaves_builds_and_round_trips() {
        let mut def = BehaviorTreeDef::default();
        let created = apply_ops(
            &mut def,
            &[
                add(DefKind::Selector, None),
                leaf(0, "game::A"),
                leaf(0, "game::B"),
            ],
        )
        .expect("applies");
        assert_eq!(created, vec![Some(0), Some(1), Some(2)]);
        assert_eq!(def.nodes[0].children, vec![1, 2]);
        assert!(is_valid(&validate(&def, None)));
        let text = to_canonical_ron(&def).expect("writes");
        assert_eq!(parse_bt_ron(&text).expect("reads"), def);
        assert_eq!(
            to_canonical_ron(&parse_bt_ron(&text).expect("reads")).expect("writes"),
            text
        );
    }

    #[test]
    fn canonical_text_matches_the_documented_layout() {
        let def = BehaviorTreeDef::selector("root", vec![]);
        let text = to_canonical_ron(&def).expect("writes");
        assert!(text.contains(
            "(name: \"root\", kind: selector, children: [], key: \"\", seconds: 0.0, times: None, task: \"\", policy: require_all),"
        ));
    }

    #[test]
    fn every_issue_code_is_reported() {
        let mut def = BehaviorTreeDef::default();
        assert_eq!(codes(&validate(&def, None)), vec!["empty_tree"]);
        def.nodes.push(DefNode {
            kind: DefKind::Inverter,
            ..DefNode::default()
        });
        assert!(codes(&validate(&def, None)).contains(&"decorator_arity"));
        def.root = 5;
        assert!(codes(&validate(&def, None)).contains(&"root_out_of_range"));
        def.root = 0;
        def.nodes[0].children = vec![9];
        assert!(codes(&validate(&def, None)).contains(&"child_out_of_range"));
        def.nodes[0].children = vec![0];
        let found = codes(&validate(&def, None));
        assert!(
            found.contains(&"cycle") && found.contains(&"shared_node"),
            "{found:?}"
        );

        let tree = BehaviorTreeDef {
            root: 0,
            nodes: vec![
                DefNode {
                    kind: DefKind::Selector,
                    children: vec![1, 2, 3],
                    ..DefNode::default()
                },
                DefNode {
                    name: "c".into(),
                    kind: DefKind::Condition,
                    children: vec![4],
                    ..DefNode::default()
                },
                DefNode {
                    name: "l".into(),
                    kind: DefKind::Leaf,
                    children: vec![4],
                    ..DefNode::default()
                },
                DefNode {
                    name: "t".into(),
                    kind: DefKind::Timeout,
                    seconds: f32::NAN,
                    children: vec![5],
                    ..DefNode::default()
                },
                DefNode {
                    name: "orphan".into(),
                    kind: DefKind::Selector,
                    ..DefNode::default()
                },
                DefNode {
                    name: "c".into(),
                    kind: DefKind::Leaf,
                    task: "no::Such".into(),
                    ..DefNode::default()
                },
            ],
            ..BehaviorTreeDef::default()
        };
        let registry = BtTaskRegistry::default();
        let found = codes(&validate(&tree, Some(&registry)));
        for want in [
            "condition_has_children",
            "empty_key",
            "leaf_has_children",
            "empty_task",
            "bad_seconds",
            "unknown_task",
            "empty_composite",
            "duplicate_name",
        ] {
            assert!(found.contains(&want), "missing {want} in {found:?}");
        }
        assert!(!is_valid(&validate(&tree, Some(&registry))));
        let mut lonely = BehaviorTreeDef::selector("r", vec![]);
        lonely.nodes.push(DefNode::default());
        let issues = validate(&lonely, None);
        assert!(is_valid(&issues), "warnings do not invalidate");
        assert!(codes(&issues).contains(&"unreachable_node"));
    }

    #[test]
    fn validation_agrees_with_the_compiler() {
        let registry = BtTaskRegistry::default();
        for def in [
            BehaviorTreeDef::default(),
            BehaviorTreeDef::selector("ok", vec![]),
            BehaviorTreeDef {
                root: 0,
                nodes: vec![DefNode {
                    children: vec![0],
                    ..DefNode::default()
                }],
                ..BehaviorTreeDef::default()
            },
        ] {
            let compiled = crate::tree::BehaviorTree::compile(&def, &registry).is_ok();
            assert_eq!(compiled, is_valid(&validate(&def, Some(&registry))), "{def:?}");
        }
    }

    #[test]
    fn connect_moves_reorders_and_refuses_bad_edges() {
        let mut def = BehaviorTreeDef::default();
        apply_ops(
            &mut def,
            &[
                add(DefKind::Selector, None),
                add(DefKind::Sequence, Some(0)),
                leaf(1, "t::A"),
                leaf(0, "t::B"),
                add(DefKind::Inverter, Some(0)),
            ],
        )
        .expect("applies");
        // Moving node 3 under the sequence removes it from the selector.
        apply_op(
            &mut def,
            &BtOp::Connect {
                parent: 1,
                child: 3,
                index: Some(0),
            },
        )
        .expect("moves");
        assert_eq!(def.nodes[1].children, vec![3, 2]);
        assert_eq!(def.nodes[0].children, vec![1, 4]);
        apply_op(
            &mut def,
            &BtOp::Reorder {
                parent: 1,
                from: 0,
                to: 1,
            },
        )
        .expect("reorders");
        assert_eq!(def.nodes[1].children, vec![2, 3]);
        let before = def.clone();
        for (op, code) in [
            (
                BtOp::Connect {
                    parent: 2,
                    child: 3,
                    index: None,
                },
                "no_children_allowed",
            ),
            (
                BtOp::Connect {
                    parent: 2,
                    child: 0,
                    index: None,
                },
                "would_cycle",
            ),
            (
                BtOp::Connect {
                    parent: 1,
                    child: 0,
                    index: None,
                },
                "would_cycle",
            ),
            (
                BtOp::Connect {
                    parent: 1,
                    child: 1,
                    index: None,
                },
                "would_cycle",
            ),
            (
                BtOp::Connect {
                    parent: 9,
                    child: 1,
                    index: None,
                },
                "bad_node",
            ),
            (
                BtOp::Reorder {
                    parent: 1,
                    from: 0,
                    to: 5,
                },
                "bad_index",
            ),
            (BtOp::Disconnect { parent: 0, child: 2 }, "not_a_child"),
            (BtOp::SetRoot { node: 9 }, "bad_root"),
        ] {
            assert_eq!(apply_op(&mut def, &op).expect_err("refused").code, code, "{op:?}");
            assert_eq!(def, before, "a refused op leaves the tree alone");
        }
        apply_op(&mut def, &leaf(4, "t::C")).expect("inverter takes one");
        assert_eq!(
            apply_op(&mut def, &leaf(4, "t::D")).expect_err("full").code,
            "decorator_full"
        );
    }

    #[test]
    fn remove_renumbers_and_subtree_takes_descendants() {
        let mut def = BehaviorTreeDef::default();
        apply_ops(
            &mut def,
            &[
                add(DefKind::Selector, None),
                add(DefKind::Sequence, Some(0)),
                leaf(1, "t::A"),
                leaf(0, "t::B"),
            ],
        )
        .expect("applies");
        let mut keep_children = def.clone();
        apply_op(
            &mut keep_children,
            &BtOp::RemoveNode {
                node: 1,
                subtree: false,
            },
        )
        .expect("removes");
        // The leaf that was node 2 is now node 1, unconnected; the other leaf is node 2.
        assert_eq!(keep_children.nodes.len(), 3);
        assert_eq!(keep_children.nodes[0].children, vec![2]);
        assert_eq!(keep_children.nodes[1].task, "t::A");
        apply_op(
            &mut def,
            &BtOp::RemoveNode {
                node: 1,
                subtree: true,
            },
        )
        .expect("removes");
        assert_eq!(def.nodes.len(), 2);
        assert_eq!(def.nodes[0].children, vec![1]);
        assert!(is_valid(&validate(&def, None)));
    }

    #[test]
    fn a_failed_batch_changes_nothing() {
        let mut def = BehaviorTreeDef::selector("r", vec![]);
        let before = def.clone();
        let err = apply_ops(
            &mut def,
            &[add(DefKind::Leaf, Some(0)), BtOp::SetRoot { node: 7 }],
        )
        .expect_err("fails");
        assert_eq!(err.op_index, 1);
        assert_eq!(err.error.code, "bad_root");
        assert_eq!(def, before);
    }

    #[test]
    fn set_node_edits_parameters() {
        let mut def = BehaviorTreeDef::default();
        apply_ops(&mut def, &[add(DefKind::Repeat, None)]).expect("applies");
        apply_op(
            &mut def,
            &BtOp::SetNode {
                node: 0,
                name: Some("loop".into()),
                kind: None,
                key: None,
                seconds: Some(1.5),
                times: Some(3),
                clear_times: false,
                task: None,
                policy: None,
                abort: None,
                op: None,
                value_kind: None,
                literal: None,
                literal_hi: None,
                clear_literal: false,
                clear_literal_hi: false,
                rhs_key: None,
            },
        )
        .expect("sets");
        assert_eq!(
            (
                def.nodes[0].name.as_str(),
                def.nodes[0].times,
                def.nodes[0].seconds
            ),
            ("loop", Some(3), 1.5)
        );
        let text = to_canonical_ron(&def).expect("writes");
        assert!(text.contains("times: Some(3)"));
        assert_eq!(parse_bt_ron(&text).expect("reads"), def);
    }
}
