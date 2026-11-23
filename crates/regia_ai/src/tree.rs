// SPDX-License-Identifier: MPL-2.0
//! Behaviour trees (`docs/specs/22-tier2.md` § T2.2): the reflected, serializable tree
//! definition (`.bt.ron`), the compiled runtime tree, and the tick logic.
//!
//! Node kinds: [`DefKind::Selector`], [`DefKind::Sequence`], [`DefKind::Parallel`] (with a
//! [`ParallelPolicy`]), the [`DefKind::Inverter`] decorator, the [`DefKind::Condition`] /
//! [`DefKind::Compare`] / [`DefKind::Cooldown`] / [`DefKind::Repeat`] / [`DefKind::Timeout`]
//! decorators, a [`DefKind::StateTree`] bridge, and [`DefKind::Leaf`] tasks (resolved from a
//! [`crate::task::BtTaskRegistration`] type path).
//!
//! The definition is a **flat node arena** (`[BehaviorTreeDef::nodes]` with a `root` index)
//! rather than a boxed recursive tree: `bevy_reflect` no longer reflects `Box<T>`, so a flat
//! arena keeps the whole thing `Reflect` + serde. A [`BehaviorTreeBuilder`] composes nodes by
//! index so hand-authored `.bt.ron` stays readable.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use thiserror::Error;

use crate::bt_value::{
    AbortMode, BtCompareOp, BtLiteral, BtValueKind, CompareFault, CompareView, compare_faults, holds,
};
use crate::state_tree::{StateTree, StateTreeDef};
use crate::task::{BtContext, BtStatus, BtTask};

thread_local! {
    /// The heights of the behavior trees currently being compiled on this thread, outermost
    /// first. A bridge compile (a `state_tree` node's asset, a state tree's `bt:` task) runs to
    /// completion inside the enclosing `compile_with`, so this stack is exactly the chain of
    /// enclosing trees — the recursion `tick` would build at run time.
    static COMPILE_HEIGHTS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The total node depth the trees enclosing this compile already consume (0 at the top level).
fn bt_depth_so_far() -> usize {
    COMPILE_HEIGHTS.with(|stack| stack.borrow().iter().sum())
}

/// Records `height` for the nested compiles this one starts; dropping the guard pops it.
fn push_compile_height(height: usize) -> CompileHeightGuard {
    COMPILE_HEIGHTS.with(|stack| stack.borrow_mut().push(height));
    CompileHeightGuard
}

/// Pops the height its `compile_with` pushed, on every exit path including errors.
struct CompileHeightGuard;

impl Drop for CompileHeightGuard {
    fn drop(&mut self) {
        COMPILE_HEIGHTS.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// How a [`DefKind::Parallel`] node decides its result.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParallelPolicy {
    /// Succeeds when every child succeeds; fails when any child fails.
    #[default]
    RequireAll,
    /// Succeeds when any child succeeds; fails when every child fails.
    RequireOne,
}

/// The most nodes a path from the root to a leaf may hold (ADR-7559).
///
/// Ticking and resetting a tree recurse one level per node, so a deeper tree is refused at compile
/// time (`BtCompileError::Malformed`) and by the validator (`too_deep`) instead of overflowing the
/// stack on its first tick. The count spans `state_tree` bridges: a bridge task's tick runs the
/// inner tree on top of the outer tree's frames, so the heights of every tree in a bridge chain
/// must sum to at most this, not merely fit one at a time.
pub const MAX_BT_DEPTH: usize = 256;

/// The kind of a behaviour-tree node.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefKind {
    /// Runs children until one succeeds (or all fail).
    #[default]
    Selector,
    /// Runs children until one fails (or all succeed).
    Sequence,
    /// Runs every child and combines with a policy.
    Parallel,
    /// Inverts a child's success/failure.
    Inverter,
    /// Succeeds while a blackboard key is `true`.
    Condition,
    /// Succeeds while a blackboard comparison holds (ADR-4543).
    Compare,
    /// Runs its child at most once every `seconds` seconds.
    Cooldown,
    /// Repeats its child until failure, or up to `times` successes.
    Repeat,
    /// Fails if its child does not finish within `seconds`.
    Timeout,
    /// A decorator that runs a Rust task every tick for side effects (state updates) and returns
    /// its child's status.
    Service,
    /// A Rust leaf task, referenced by its registered type path.
    Leaf,
    /// Runs a `.statetree.ron` to completion (ADR-4543). The path is [`DefNode::task`].
    StateTree,
}

/// One node of the flat arena. Every node carries the fields its kind needs; the others are
/// ignored (kept on one struct so the type is a single reflectable shape).
#[derive(Reflect, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DefNode {
    /// Debug name (editor / diagnostics).
    pub name: String,
    /// What the node does.
    pub kind: DefKind,
    /// Child node indices (decorators take exactly one; leaves take none).
    pub children: Vec<usize>,
    /// The blackboard key for a [`DefKind::Condition`].
    pub key: String,
    /// The timing budget (seconds) for [`DefKind::Cooldown`] / [`DefKind::Timeout`].
    pub seconds: f32,
    /// The success cap for a [`DefKind::Repeat`].
    pub times: Option<u32>,
    /// The task type path for a [`DefKind::Leaf`] or [`DefKind::Service`].
    pub task: String,
    /// The policy for a [`DefKind::Parallel`].
    pub policy: ParallelPolicy,
    /// When a change to a watched key aborts a running branch (ADR-4543). Default: do not watch.
    #[serde(default)]
    pub abort: AbortMode,
    /// The relation of a [`DefKind::Compare`].
    #[serde(default)]
    pub op: BtCompareOp,
    /// The type a [`DefKind::Compare`] reads.
    #[serde(default)]
    pub value_kind: BtValueKind,
    /// The constant, or the low bound of a range.
    #[serde(default)]
    pub literal: Option<BtLiteral>,
    /// The high bound of a range.
    #[serde(default)]
    pub literal_hi: Option<BtLiteral>,
    /// A second blackboard key. Empty means [`DefNode::literal`] is the right-hand side.
    #[serde(default)]
    pub rhs_key: String,
}

impl Default for DefNode {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: DefKind::Selector,
            children: Vec::new(),
            key: String::new(),
            seconds: 0.0,
            times: None,
            task: String::new(),
            policy: ParallelPolicy::default(),
            abort: AbortMode::None,
            op: BtCompareOp::Eq,
            value_kind: BtValueKind::Bool,
            literal: None,
            literal_hi: None,
            rhs_key: String::new(),
        }
    }
}

/// Whether `node` stores anything `.bt.ron` version 2 added.
pub(crate) fn node_uses_v2(node: &DefNode) -> bool {
    node.abort != AbortMode::None
        || matches!(node.kind, DefKind::Compare | DefKind::StateTree)
        || node.op != BtCompareOp::Eq
        || node.value_kind != BtValueKind::Bool
        || node.literal.is_some()
        || node.literal_hi.is_some()
        || !node.rhs_key.is_empty()
}

/// The blackboard keys a node with [`AbortMode`] other than `None` watches.
pub(crate) fn observed_keys(node: &DefNode) -> Vec<&str> {
    let mut keys = Vec::new();
    if !node.key.is_empty() {
        keys.push(node.key.as_str());
    }
    if node.kind == DefKind::Compare && !node.rhs_key.is_empty() {
        keys.push(node.rhs_key.as_str());
    }
    keys
}

/// An authored behaviour tree: a flat node arena plus the root index. Reflect + serde (`.bt.ron`).
#[derive(Component, Reflect, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(Component, Default)]
pub struct BehaviorTreeDef {
    /// The file shape. Absent in a v1 file, which reads as 1 (ADR-4543).
    #[serde(default = "bt_format_version_default")]
    pub format_version: u32,
    /// The root node index.
    pub root: usize,
    /// The node arena (children reference other nodes by index).
    pub nodes: Vec<DefNode>,
}

fn bt_format_version_default() -> u32 {
    1
}

impl Default for BehaviorTreeDef {
    fn default() -> Self {
        Self {
            format_version: 1,
            root: 0,
            nodes: Vec::new(),
        }
    }
}

/// The version of the `.bt.ron` shape. v1 files have no `format_version` and still load.
/// v2 adds value conditions, observer aborts and the state-tree bridge (ADR-4543).
pub const BT_FORMAT_VERSION: u32 = 2;

/// The serde shape of `.bt.ron` for `cargo xtask check-formats` (`docs/formats/bt-ron.md`, ADR-2316,
/// ADR-4543): the field names of [`BehaviorTreeDef`] and [`DefNode`] and the enum spellings.
#[must_use]
pub fn bt_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<BehaviorTreeDef>(),
        serde_field_names::<DefNode>(),
        serde_variant_names::<DefKind>(),
        serde_variant_names::<ParallelPolicy>(),
        serde_variant_names::<AbortMode>(),
        serde_variant_names::<BtCompareOp>(),
        serde_variant_names::<BtValueKind>(),
        serde_variant_names::<BtLiteral>(),
    ])
}

/// Versions a migrator must still read. v1 has no `format_version` field (ADR-4543).
#[must_use]
pub fn bt_migration_sources() -> Vec<u32> {
    vec![1]
}

impl BehaviorTreeDef {
    /// Builds a selector named `name` over `children` (already-compiled child indices).
    pub fn selector(name: impl Into<String>, children: Vec<usize>) -> Self {
        let mut nodes = Vec::with_capacity(children.len() + 1);
        let root = nodes.len();
        nodes.push(DefNode {
            name: name.into(),
            kind: DefKind::Selector,
            children,
            ..Default::default()
        });
        Self {
            format_version: 1,
            root,
            nodes,
        }
    }

    /// A sequence named `name` over `children`.
    pub fn sequence(name: impl Into<String>, children: Vec<usize>) -> Self {
        let mut nodes = Vec::with_capacity(children.len() + 1);
        let root = nodes.len();
        nodes.push(DefNode {
            name: name.into(),
            kind: DefKind::Sequence,
            children,
            ..Default::default()
        });
        Self {
            format_version: 1,
            root,
            nodes,
        }
    }

    /// A leaf task named `name`.
    pub fn leaf(name: impl Into<String>, task: impl Into<String>) -> Self {
        let mut nodes = Vec::with_capacity(1);
        let root = nodes.len();
        nodes.push(DefNode {
            name: name.into(),
            kind: DefKind::Leaf,
            task: task.into(),
            ..Default::default()
        });
        Self {
            format_version: 1,
            root,
            nodes,
        }
    }

    /// A condition on blackboard key `key`.
    pub fn condition(name: impl Into<String>, key: impl Into<String>) -> Self {
        let mut nodes = Vec::with_capacity(1);
        let root = nodes.len();
        nodes.push(DefNode {
            name: name.into(),
            kind: DefKind::Condition,
            key: key.into(),
            ..Default::default()
        });
        Self {
            format_version: 1,
            root,
            nodes,
        }
    }

    /// A decorator named `name` around a single `child` index.
    pub fn decorated(kind: DefKind, name: impl Into<String>, child: usize) -> Self {
        let mut nodes = Vec::with_capacity(2);
        let root = nodes.len();
        nodes.push(DefNode {
            name: name.into(),
            kind,
            children: vec![child],
            ..Default::default()
        });
        Self {
            format_version: 1,
            root,
            nodes,
        }
    }
}

/// Composes a [`BehaviorTreeDef`] by appending nodes and referring to them by index.
#[derive(Default)]
pub struct BehaviorTreeBuilder {
    /// The arena being built.
    pub nodes: Vec<DefNode>,
}

impl BehaviorTreeBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a node and returns its index.
    pub fn add(&mut self, node: DefNode) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        index
    }

    /// Appends a selector and returns its index.
    pub fn selector(&mut self, name: impl Into<String>, children: Vec<usize>) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Selector,
            children,
            ..Default::default()
        })
    }

    /// Appends a sequence and returns its index.
    pub fn sequence(&mut self, name: impl Into<String>, children: Vec<usize>) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Sequence,
            children,
            ..Default::default()
        })
    }

    /// Appends a parallel node and returns its index.
    pub fn parallel(
        &mut self,
        name: impl Into<String>,
        policy: ParallelPolicy,
        children: Vec<usize>,
    ) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Parallel,
            children,
            policy,
            ..Default::default()
        })
    }

    /// Appends an inverter around `child` and returns its index.
    pub fn inverter(&mut self, name: impl Into<String>, child: usize) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Inverter,
            children: vec![child],
            ..Default::default()
        })
    }

    /// Appends a condition and returns its index.
    pub fn condition(&mut self, name: impl Into<String>, key: impl Into<String>) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Condition,
            key: key.into(),
            ..Default::default()
        })
    }

    /// Appends a cooldown decorator around `child` and returns its index.
    pub fn cooldown(&mut self, name: impl Into<String>, seconds: f32, child: usize) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Cooldown,
            children: vec![child],
            seconds,
            ..Default::default()
        })
    }

    /// Appends a repeat decorator around `child` and returns its index.
    pub fn repeat(&mut self, name: impl Into<String>, times: Option<u32>, child: usize) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Repeat,
            children: vec![child],
            times,
            ..Default::default()
        })
    }

    /// Appends a timeout decorator around `child` and returns its index.
    pub fn timeout(&mut self, name: impl Into<String>, seconds: f32, child: usize) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Timeout,
            children: vec![child],
            seconds,
            ..Default::default()
        })
    }

    /// Appends a service decorator (task `task` run every tick, child `child`) and returns its
    /// index.
    pub fn service(&mut self, name: impl Into<String>, task: impl Into<String>, child: usize) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Service,
            children: vec![child],
            task: task.into(),
            ..Default::default()
        })
    }

    /// Appends a leaf task and returns its index.
    pub fn leaf(&mut self, name: impl Into<String>, task: impl Into<String>) -> usize {
        self.add(DefNode {
            name: name.into(),
            kind: DefKind::Leaf,
            task: task.into(),
            ..Default::default()
        })
    }

    /// Finishes the builder with `root` as the root node.
    ///
    /// The format version is 2 when any node uses a v2 field, and 1 otherwise.
    pub fn build(self, root: usize) -> BehaviorTreeDef {
        let format_version = if self.nodes.iter().any(node_uses_v2) { 2 } else { 1 };
        BehaviorTreeDef {
            format_version,
            root,
            nodes: self.nodes,
        }
    }
}

/// A registry of leaf-task constructors, keyed by type path (see
/// [`crate::task::BtTaskRegistration`]).
#[derive(Resource, Default)]
pub struct BtTaskRegistry {
    /// Type path → constructor.
    constructors: std::collections::BTreeMap<String, Box<dyn Fn() -> Box<dyn BtTask> + Send + Sync>>,
    /// Task paths marked presentation (logic graphs run them on a peer without authority too).
    presentation: std::collections::BTreeSet<String>,
}

impl BtTaskRegistry {
    /// Registers a leaf task type (its type path is its lookup key).
    pub fn register<T: crate::task::BtTaskRegistration + 'static>(&mut self) {
        self.constructors
            .insert(T::task_type_path().to_string(), Box::new(T::construct));
    }

    /// Registers a configured leaf under `task_path`: `factory` builds a fresh instance each time a
    /// tree using that path compiles (for tasks with settings, such as
    /// [`crate::spatial_query::RunSpatialQuery`]). Replaces an earlier registration of the path.
    pub fn register_factory(
        &mut self,
        task_path: impl Into<String>,
        factory: impl Fn() -> Box<dyn BtTask> + Send + Sync + 'static,
    ) {
        self.constructors.insert(task_path.into(), Box::new(factory));
    }

    /// Marks `task_path` a *presentation* task (spec 102 § 8): it only drives local sight and sound
    /// (a sequence, a sound cue), so a logic graph on a peer without authority still runs it, where
    /// it skips every other (sim) action. The mark is independent of the registration.
    pub fn mark_presentation(&mut self, task_path: impl Into<String>) {
        self.presentation.insert(task_path.into());
    }

    /// Whether `task_path` is marked presentation ([`BtTaskRegistry::mark_presentation`]).
    pub fn is_presentation(&self, task_path: &str) -> bool {
        self.presentation.contains(task_path)
    }

    /// Constructs the task registered under `type_path`, if any.
    pub fn construct(&self, type_path: &str) -> Option<Box<dyn BtTask>> {
        self.constructors.get(type_path).map(|ctor| ctor())
    }

    /// The registered task type paths, sorted (the editor's leaf palette).
    pub fn task_paths(&self) -> impl Iterator<Item = &str> {
        self.constructors.keys().map(String::as_str)
    }

    /// How many task types are registered.
    pub fn len(&self) -> usize {
        self.constructors.len()
    }

    /// Whether no task types are registered.
    pub fn is_empty(&self) -> bool {
        self.constructors.is_empty()
    }
}

/// Why a tree failed to compile from its definition.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum BtCompileError {
    /// A leaf referenced a task type path that is not registered.
    #[error("behavior tree leaf {0:?} references unregistered task type {1:?}")]
    UnknownTask(String, String),
    /// A node references a child index outside the arena, or a decorator has the wrong arity.
    #[error("behavior tree is malformed: {0}")]
    Malformed(String),
    /// A `state_tree` node could not load or compile its asset.
    #[error("behavior tree node {node:?} could not run state tree {path:?}: {detail}")]
    BadStateTree {
        /// The node's name.
        node: String,
        /// The asset path.
        path: String,
        /// Why it failed.
        detail: String,
    },
}

/// A compiled behaviour tree: the resolved leaf tasks plus the per-node runtime state.
///
/// Build from a [`BehaviorTreeDef`] with [`BehaviorTree::compile`]; tick with
/// [`BehaviorTree::tick`] (driven each `FixedUpdate` by [`crate::AiPlugin`]).
#[derive(Component)]
pub struct BehaviorTree {
    /// The authored definition this tree was compiled from (for inspection).
    pub def: BehaviorTreeDef,
    /// The node arena (parallel to `state`; indices match the definition's arena).
    nodes: Vec<BtNode>,
    /// Per-node runtime state (parallel to `nodes`).
    state: Vec<NodeState>,
    /// The status of the last tick (the root's result).
    pub last_status: BtStatus,
    /// The per-node status from the last tick (parallel to `nodes`).
    node_status: Vec<BtStatus>,
    /// Nodes whose [`AbortMode`] is not `None`, in arena order.
    observers: Vec<usize>,
    /// The union of the keys those nodes watch.
    watched_keys: Vec<String>,
    /// The last value seen for each watched key (`None` when the key was absent).
    snapshot: BTreeMap<String, Option<crate::blackboard::BbValue>>,
    /// How many observers have been re-checked because a watched key changed.
    observer_evaluations: u64,
    /// The first tick only records the snapshot. It does not count as an evaluation.
    baselined: bool,
    /// Set after the first tick, so a decorator does not abort on the initial all-`Running` statuses.
    started: bool,
    /// Task nodes already reset during this observer dispatch (not ordinary control-flow resets).
    observer_resets: Option<BTreeSet<usize>>,
}

/// A reflected, inspectable view of a behaviour tree's last tick: the root status plus each node's
/// status. Written every tick by [`crate::AiPlugin`] next to the [`BehaviorTree`] so the editor and
/// the agent protocol can read the running tree without the non-reflectable [`BehaviorTree`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct BehaviorTreeStatus {
    /// The root's status from the last tick.
    pub last_status: BtStatus,
    /// Per-node status from the last tick, parallel to [`BehaviorTreeDef::nodes`].
    pub node_status: Vec<BtStatus>,
}

/// The compiled form of one node.
enum BtNode {
    Selector(Vec<usize>),
    Sequence(Vec<usize>),
    Parallel {
        children: Vec<usize>,
        policy: ParallelPolicy,
    },
    Inverter(usize),
    Condition(String),
    Cooldown {
        seconds: f32,
        child: usize,
    },
    Repeat {
        times: Option<u32>,
        child: usize,
    },
    Timeout {
        seconds: f32,
        child: usize,
    },
    Service {
        task: Box<dyn BtTask>,
        child: usize,
    },
    Leaf(Box<dyn BtTask>),
    Compare,
    StateTree(Box<StateTree>),
}

/// The runtime state of one node.
#[derive(Default, Clone)]
struct NodeState {
    /// Whether the node is still running, unlike its historical debugger status after a reset.
    active: bool,
    /// The child index a selector/sequence is resuming from.
    running_child: Option<usize>,
    /// Remaining cooldown (seconds).
    cooldown_remaining: f64,
    /// Successes counted by a repeat node.
    repeat_count: u32,
    /// Elapsed time in a timeout node.
    timeout_elapsed: f64,
    /// Per-child latched result of a parallel node (`Some(true)` = succeeded).
    parallel_done: Vec<Option<bool>>,
    /// A status the observer abort forced for this tick, consumed by the next visit.
    forced: Option<BtStatus>,
}

impl BehaviorTree {
    /// Compiles a tree definition, resolving every leaf against `registry`.
    ///
    /// Fails with [`BtCompileError::Malformed`] when the root or a child index is outside the
    /// arena (including an empty arena) or the nodes reachable from the root form a cycle, and
    /// with [`BtCompileError::UnknownTask`] for an unregistered leaf.
    pub fn compile(def: &BehaviorTreeDef, registry: &BtTaskRegistry) -> Result<Self, BtCompileError> {
        Self::compile_with(def, registry, &|_| None)
    }

    /// Like [`BehaviorTree::compile`], loading a `state_tree` node's `.statetree.ron` through `load`.
    ///
    /// `load` receives the project-relative path and returns the file text. A missing asset is
    /// [`BtCompileError::BadStateTree`].
    ///
    /// A tree compiled while bridges are open (a `state_tree` node, or a state tree's `bt:` task)
    /// counts the enclosing trees' heights against [`MAX_BT_DEPTH`]: a bridge task's tick runs
    /// the inner tree on top of the outer tree's recursion frames, so the cap spans the chain.
    pub fn compile_with(
        def: &BehaviorTreeDef,
        registry: &BtTaskRegistry,
        load: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, BtCompileError> {
        let base = bt_depth_so_far();
        let height = validate_graph(def, base)?;
        let _height = push_compile_height(height);
        let mut nodes = Vec::with_capacity(def.nodes.len());
        for (index, node) in def.nodes.iter().enumerate() {
            nodes.push(compile_def_node(index, node, registry, def.nodes.len(), load)?);
        }
        let observers: Vec<usize> = def
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.abort != AbortMode::None)
            .map(|(index, _)| index)
            .collect();
        let mut watched_keys = Vec::new();
        for &index in &observers {
            for key in observed_keys(&def.nodes[index]) {
                if !watched_keys.iter().any(|have: &String| have == key) {
                    watched_keys.push(key.to_string());
                }
            }
        }
        Ok(Self {
            def: def.clone(),
            state: vec![NodeState::default(); nodes.len()],
            nodes,
            last_status: BtStatus::Running,
            node_status: vec![BtStatus::Running; def.nodes.len()],
            observers,
            watched_keys,
            snapshot: BTreeMap::new(),
            observer_evaluations: 0,
            baselined: false,
            started: false,
            observer_resets: None,
        })
    }

    /// How many observers have been re-checked because one of their keys changed.
    ///
    /// The first tick records a baseline and does not count. A later tick with no changed watched
    /// key does not count either.
    #[must_use]
    pub fn observer_evaluations(&self) -> u64 {
        self.observer_evaluations
    }

    /// Clears runtime state and calls [`BtTask::reset`] on every leaf and service, as an abort does.
    pub fn reset(&mut self, ctx: &mut BtContext<'_>) {
        if self.def.root < self.nodes.len() {
            self.reset_subtree(self.def.root, ctx);
        }
        self.last_status = BtStatus::Running;
        self.node_status.fill(BtStatus::Running);
        self.snapshot.clear();
        self.baselined = false;
        self.started = false;
    }

    /// Ticks the tree once.
    pub fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        self.dispatch_observers(ctx);
        self.last_status = self.tick_node(self.def.root, ctx);
        self.started = true;
        self.last_status
    }

    /// The per-node status from the last tick (parallel to [`BehaviorTreeDef::nodes`]).
    pub fn node_statuses(&self) -> &[BtStatus] {
        &self.node_status
    }

    fn tick_node(&mut self, idx: usize, ctx: &mut BtContext<'_>) -> BtStatus {
        let status = match self.state[idx].forced.take() {
            Some(status) => status,
            None => self.tick_node_inner(idx, ctx),
        };
        self.node_status[idx] = status;
        self.state[idx].active = status == BtStatus::Running;
        status
    }

    fn child_count(&self, idx: usize) -> usize {
        match &self.nodes[idx] {
            BtNode::Selector(children) | BtNode::Sequence(children) | BtNode::Parallel { children, .. } => {
                children.len()
            }
            BtNode::Inverter(_)
            | BtNode::Cooldown { .. }
            | BtNode::Repeat { .. }
            | BtNode::Timeout { .. }
            | BtNode::Service { .. } => 1,
            BtNode::Leaf(_) | BtNode::Condition(_) | BtNode::Compare | BtNode::StateTree(_) => 0,
        }
    }

    fn child_at(&self, idx: usize, nth: usize) -> usize {
        match &self.nodes[idx] {
            BtNode::Selector(children) | BtNode::Sequence(children) | BtNode::Parallel { children, .. } => {
                children[nth]
            }
            BtNode::Inverter(child)
            | BtNode::Cooldown { child, .. }
            | BtNode::Repeat { child, .. }
            | BtNode::Timeout { child, .. }
            | BtNode::Service { child, .. } => *child,
            BtNode::Leaf(_) | BtNode::Condition(_) | BtNode::Compare | BtNode::StateTree(_) => {
                unreachable!("node {idx} has no children")
            }
        }
    }

    fn tick_node_inner(&mut self, idx: usize, ctx: &mut BtContext<'_>) -> BtStatus {
        // A decorator that watches a key is a gate (ADR-7559): entering it from rest checks the
        // key, and a shut gate fails without touching its child, task or timers. While it runs, the
        // key is re-checked only when it changes (`abort_self`).
        if !self.state[idx].active && self.gate_shut(idx, ctx) {
            return BtStatus::Failure;
        }
        // Leaves and services tick their embedded task directly (they need `&mut World` through
        // `ctx`, not a re-borrow of `self`).
        if let BtNode::Leaf(task) = &mut self.nodes[idx] {
            return task.tick(ctx);
        }
        if let BtNode::StateTree(tree) = &mut self.nodes[idx] {
            return crate::bt_bridge::map_state_status(tree.tick(ctx));
        }
        if let BtNode::Service { task, child } = &mut self.nodes[idx] {
            // The service task runs every tick for side effects; its status is ignored.
            let _ = task.tick(ctx);
            let child = *child;
            // Tick the child through `tick_node` so its per-node status is recorded for the
            // reflected snapshot (a bare `tick_node_inner` would leave it `Running`).
            return self.tick_node(child, ctx);
        }

        // Copy only the `Copy` fields. Child indices and condition keys stay in `self.nodes` and
        // are re-read after each recursive tick, so a steady tick does not clone them.
        enum Kind {
            Selector,
            Sequence,
            Parallel(ParallelPolicy),
            Inverter,
            Condition,
            Compare,
            Cooldown(f32),
            Repeat(Option<u32>),
            Timeout(f32),
        }
        let kind = match &self.nodes[idx] {
            BtNode::Selector(_) => Kind::Selector,
            BtNode::Sequence(_) => Kind::Sequence,
            BtNode::Parallel { policy, .. } => Kind::Parallel(*policy),
            BtNode::Inverter(_) => Kind::Inverter,
            BtNode::Condition(_) => Kind::Condition,
            BtNode::Compare => Kind::Compare,
            BtNode::Cooldown { seconds, .. } => Kind::Cooldown(*seconds),
            BtNode::Repeat { times, .. } => Kind::Repeat(*times),
            BtNode::Timeout { seconds, .. } => Kind::Timeout(*seconds),
            BtNode::Service { .. } | BtNode::Leaf(_) | BtNode::StateTree(_) => unreachable!("handled above"),
        };

        match kind {
            Kind::Selector => {
                let count = self.child_count(idx);
                let start = self.state[idx].running_child.unwrap_or(0).min(count);
                for nth in start..count {
                    let child = self.child_at(idx, nth);
                    match self.tick_node(child, ctx) {
                        BtStatus::Success => {
                            self.state[idx].running_child = None;
                            return BtStatus::Success;
                        }
                        BtStatus::Running => {
                            self.state[idx].running_child = Some(nth);
                            return BtStatus::Running;
                        }
                        BtStatus::Failure => {}
                    }
                }
                self.state[idx].running_child = None;
                BtStatus::Failure
            }
            Kind::Sequence => {
                let count = self.child_count(idx);
                let start = self.state[idx].running_child.unwrap_or(0).min(count);
                for nth in start..count {
                    let child = self.child_at(idx, nth);
                    match self.tick_node(child, ctx) {
                        BtStatus::Failure => {
                            self.state[idx].running_child = None;
                            return BtStatus::Failure;
                        }
                        BtStatus::Running => {
                            self.state[idx].running_child = Some(nth);
                            return BtStatus::Running;
                        }
                        BtStatus::Success => {}
                    }
                }
                self.state[idx].running_child = None;
                BtStatus::Success
            }
            Kind::Parallel(policy) => {
                // A child that finished stays finished until the parallel does: it is not ticked
                // again, so a one-shot sibling cannot fire twice or flip its result.
                let count = self.child_count(idx);
                if self.state[idx].parallel_done.len() != count {
                    self.state[idx].parallel_done.clear();
                    self.state[idx].parallel_done.resize(count, None);
                }
                for nth in 0..count {
                    if self.state[idx].parallel_done[nth].is_some() {
                        continue;
                    }
                    let child = self.child_at(idx, nth);
                    match self.tick_node(child, ctx) {
                        BtStatus::Success => self.state[idx].parallel_done[nth] = Some(true),
                        BtStatus::Failure => self.state[idx].parallel_done[nth] = Some(false),
                        BtStatus::Running => {}
                    }
                }
                let successes = self.state[idx]
                    .parallel_done
                    .iter()
                    .filter(|done| **done == Some(true))
                    .count();
                let failures = self.state[idx]
                    .parallel_done
                    .iter()
                    .filter(|done| **done == Some(false))
                    .count();
                let result = match policy {
                    ParallelPolicy::RequireAll => {
                        if failures > 0 {
                            BtStatus::Failure
                        } else if successes == count {
                            BtStatus::Success
                        } else {
                            BtStatus::Running
                        }
                    }
                    ParallelPolicy::RequireOne => {
                        if successes > 0 {
                            BtStatus::Success
                        } else if failures == count {
                            BtStatus::Failure
                        } else {
                            BtStatus::Running
                        }
                    }
                };
                if result != BtStatus::Running {
                    // Finished: halt the siblings still working and clear the latches.
                    for nth in 0..count {
                        let child = self.child_at(idx, nth);
                        self.reset_subtree(child, ctx);
                    }
                    self.state[idx].parallel_done.clear();
                }
                result
            }
            Kind::Inverter => {
                let child = self.child_at(idx, 0);
                match self.tick_node(child, ctx) {
                    BtStatus::Success => BtStatus::Failure,
                    BtStatus::Failure => BtStatus::Success,
                    BtStatus::Running => BtStatus::Running,
                }
            }
            Kind::Condition => {
                let holds = match &self.nodes[idx] {
                    BtNode::Condition(key) => ctx.blackboard().is_some_and(|bb| bb.get_bool(key)),
                    _ => false,
                };
                if holds {
                    BtStatus::Success
                } else {
                    BtStatus::Failure
                }
            }
            Kind::Compare => {
                let holds = ctx
                    .blackboard()
                    .is_some_and(|bb| holds(bb, compare_view(&self.def.nodes[idx])));
                if holds {
                    BtStatus::Success
                } else {
                    BtStatus::Failure
                }
            }
            Kind::Cooldown(seconds) => {
                if self.state[idx].cooldown_remaining > 0.0 {
                    self.state[idx].cooldown_remaining -= f64::from(ctx.dt);
                    if self.state[idx].cooldown_remaining > 0.0 {
                        return BtStatus::Failure;
                    }
                    self.state[idx].cooldown_remaining = 0.0;
                }
                let child = self.child_at(idx, 0);
                match self.tick_node(child, ctx) {
                    BtStatus::Success => {
                        self.state[idx].cooldown_remaining = f64::from(seconds);
                        BtStatus::Success
                    }
                    other => other,
                }
            }
            Kind::Repeat(Some(0)) => BtStatus::Success,
            Kind::Repeat(times) => {
                let child = self.child_at(idx, 0);
                match self.tick_node(child, ctx) {
                    BtStatus::Running => BtStatus::Running,
                    BtStatus::Failure => {
                        self.state[idx].repeat_count = 0;
                        self.reset_subtree(child, ctx);
                        BtStatus::Failure
                    }
                    BtStatus::Success => {
                        self.state[idx].repeat_count += 1;
                        if times.is_some_and(|n| self.state[idx].repeat_count >= n) {
                            self.state[idx].repeat_count = 0;
                            self.reset_subtree(child, ctx);
                            BtStatus::Success
                        } else {
                            self.reset_subtree(child, ctx);
                            BtStatus::Running
                        }
                    }
                }
            }
            Kind::Timeout(seconds) => {
                // The child is ticked first, so a budget of N steps runs it N times.
                let child = self.child_at(idx, 0);
                let status = self.tick_node(child, ctx);
                if status != BtStatus::Running {
                    self.state[idx].timeout_elapsed = 0.0;
                    return status;
                }
                self.state[idx].timeout_elapsed += f64::from(ctx.dt);
                if self.state[idx].timeout_elapsed >= f64::from(seconds) {
                    self.state[idx].timeout_elapsed = 0.0;
                    self.reset_subtree(child, ctx);
                    return BtStatus::Failure;
                }
                BtStatus::Running
            }
        }
    }

    /// Whether `idx` is a decorator with an observer whose check fails now. A missing blackboard
    /// fails the check, as it does for an abort.
    fn gate_shut(&self, idx: usize, ctx: &BtContext<'_>) -> bool {
        let node = &self.def.nodes[idx];
        node.abort != AbortMode::None
            && is_abort_decorator(node.kind)
            && !ctx.blackboard().is_some_and(|board| observer_passes(node, board))
    }

    /// Resets the runtime state of `idx` and all its descendants, and tells every leaf and
    /// service task in the subtree to `reset`.
    fn reset_subtree(&mut self, idx: usize, ctx: &mut BtContext<'_>) {
        let reset_task = self.observer_resets.as_mut().is_none_or(|seen| seen.insert(idx));
        self.state[idx] = NodeState::default();
        match &mut self.nodes[idx] {
            BtNode::Service { task, .. } | BtNode::Leaf(task) if reset_task => task.reset(ctx),
            BtNode::StateTree(tree) if reset_task => tree.restart(ctx),
            _ => {}
        }
        let count = self.child_count(idx);
        for nth in 0..count {
            let child = self.child_at(idx, nth);
            self.reset_subtree(child, ctx);
        }
    }

    /// Re-checks observers when a watched key changed, and aborts the running branch.
    ///
    /// A tree with no observers does not touch the blackboard. The first call only stores a
    /// snapshot. Later calls do nothing when no watched key changed, and they do not enter the
    /// dispatch span in that case.
    fn dispatch_observers(&mut self, ctx: &mut BtContext<'_>) {
        if self.observers.is_empty() {
            return;
        }
        if !self.baselined {
            self.write_snapshot(ctx);
            if let Some(mut board) = ctx.blackboard_mut() {
                board.drain_changes();
            }
            self.baselined = true;
            return;
        }
        let dirty = self.dirty_keys(ctx);
        let hit: Vec<usize> = self
            .observers
            .iter()
            .copied()
            .filter(|index| {
                observed_keys(&self.def.nodes[*index])
                    .into_iter()
                    .any(|key| dirty.contains(key))
            })
            .collect();
        if hit.is_empty() {
            return;
        }
        let span = tracing::info_span!(target: "regia_ai", "bt_observer_dispatch");
        let _entered = span.enter();
        self.observer_evaluations = self.observer_evaluations.saturating_add(hit.len() as u64);
        self.observer_resets = Some(BTreeSet::new());
        self.abort_lower(ctx, &hit);
        self.abort_self(ctx, &hit);
        self.observer_resets = None;
    }

    fn dirty_keys(&mut self, ctx: &mut BtContext<'_>) -> BTreeSet<String> {
        let mut dirty = BTreeSet::new();
        if let Some(mut board) = ctx.blackboard_mut() {
            dirty = board.drain_changes();
        }
        if let Some(board) = ctx.blackboard() {
            for key in &self.watched_keys {
                let now = board.get(key).cloned();
                let same = match self.snapshot.get(key) {
                    Some(Some(old)) => now
                        .as_ref()
                        .is_some_and(|value| crate::blackboard::bb_value_same(old, value)),
                    Some(None) => now.is_none(),
                    None => now.is_none(),
                };
                if !same {
                    dirty.insert(key.clone());
                }
            }
        }
        self.write_snapshot(ctx);
        dirty
    }

    fn write_snapshot(&mut self, ctx: &BtContext<'_>) {
        let Some(board) = ctx.blackboard() else {
            for key in &self.watched_keys {
                self.snapshot.insert(key.clone(), None);
            }
            return;
        };
        for key in &self.watched_keys {
            self.snapshot.insert(key.clone(), board.get(key).cloned());
        }
    }

    fn abort_lower(&mut self, ctx: &mut BtContext<'_>, hit: &[usize]) {
        let selectors: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| matches!(node, BtNode::Selector(_)).then_some(index))
            .collect();
        let mut victims = Vec::new();
        let mut jumps = Vec::new();
        for sel in selectors {
            let Some(running_at) = self.state[sel].running_child else {
                continue;
            };
            let children = match &self.nodes[sel] {
                BtNode::Selector(children) => children.clone(),
                _ => continue,
            };
            if running_at >= children.len() {
                continue;
            }
            let mut earliest = None;
            for (index, &child) in children.iter().enumerate() {
                if index >= running_at {
                    break;
                }
                if self.subtree_lower_passes(child, hit, ctx) {
                    earliest = Some(index);
                    break;
                }
            }
            if let Some(index) = earliest {
                victims.push(children[running_at]);
                jumps.push((sel, index));
            }
        }
        let victims = outermost(&self.def, victims);
        for victim in &victims {
            self.reset_subtree(*victim, ctx);
        }
        for (sel, index) in jumps {
            if victims
                .iter()
                .any(|victim| contains_node(&self.def, *victim, sel))
            {
                continue;
            }
            self.state[sel].running_child = Some(index);
        }
    }

    fn abort_self(&mut self, ctx: &mut BtContext<'_>, hit: &[usize]) {
        let sequences: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| matches!(node, BtNode::Sequence(_)).then_some(index))
            .collect();
        for seq in sequences {
            let Some(running_at) = self.state[seq].running_child else {
                continue;
            };
            let children = match &self.nodes[seq] {
                BtNode::Sequence(children) => children.clone(),
                _ => continue,
            };
            if running_at == 0 || running_at >= children.len() {
                continue;
            }
            let failed = children[..running_at]
                .iter()
                .any(|child| self.subtree_self_fails(*child, hit, ctx));
            if failed {
                let running = children[running_at];
                self.reset_subtree(running, ctx);
                self.state[seq].running_child = None;
                self.state[seq].forced = Some(BtStatus::Failure);
            }
        }
        if !self.started {
            return;
        }
        let decorators: Vec<usize> = hit
            .iter()
            .copied()
            .filter(|index| {
                let node = &self.def.nodes[*index];
                matches!(node.abort, AbortMode::AbortSelf | AbortMode::Both) && is_abort_decorator(node.kind)
            })
            .collect();
        for index in decorators {
            if !self.state[index].active {
                continue;
            }
            let passes = ctx
                .blackboard()
                .is_some_and(|board| observer_passes(&self.def.nodes[index], board));
            if passes {
                continue;
            }
            self.reset_subtree(index, ctx);
            self.state[index].forced = Some(BtStatus::Failure);
        }
    }

    fn subtree_lower_passes(&self, root: usize, hit: &[usize], ctx: &BtContext<'_>) -> bool {
        let Some(board) = ctx.blackboard() else {
            return false;
        };
        self.walk(root, |index| {
            if !hit.contains(&index) {
                return false;
            }
            let node = &self.def.nodes[index];
            matches!(node.abort, AbortMode::LowerPriority | AbortMode::Both) && observer_passes(node, board)
        })
    }

    fn subtree_self_fails(&self, root: usize, hit: &[usize], ctx: &BtContext<'_>) -> bool {
        let Some(board) = ctx.blackboard() else {
            return false;
        };
        self.walk(root, |index| {
            if !hit.contains(&index) {
                return false;
            }
            let node = &self.def.nodes[index];
            let condition = matches!(node.kind, DefKind::Condition | DefKind::Compare);
            condition
                && matches!(node.abort, AbortMode::AbortSelf | AbortMode::Both)
                && !observer_passes(node, board)
        })
    }

    fn walk(&self, root: usize, mut pred: impl FnMut(usize) -> bool) -> bool {
        let mut stack = vec![root];
        let mut seen = vec![false; self.def.nodes.len()];
        while let Some(index) = stack.pop() {
            if index >= self.def.nodes.len() || seen[index] {
                continue;
            }
            seen[index] = true;
            if pred(index) {
                return true;
            }
            stack.extend(self.def.nodes[index].children.iter().copied());
        }
        false
    }
}

fn is_abort_decorator(kind: DefKind) -> bool {
    matches!(
        kind,
        DefKind::Inverter | DefKind::Cooldown | DefKind::Repeat | DefKind::Timeout | DefKind::Service
    )
}

fn compare_view(node: &DefNode) -> CompareView<'_> {
    CompareView {
        op: node.op,
        kind: node.value_kind,
        key: &node.key,
        rhs_key: &node.rhs_key,
        literal: &node.literal,
        literal_hi: &node.literal_hi,
    }
}

fn observer_passes(node: &DefNode, board: &crate::blackboard::Blackboard) -> bool {
    if node.kind == DefKind::Compare {
        holds(board, compare_view(node))
    } else {
        board.get_bool(&node.key)
    }
}

/// Drops a node that sits inside another node of `nodes`.
fn outermost(def: &BehaviorTreeDef, nodes: Vec<usize>) -> Vec<usize> {
    nodes
        .iter()
        .copied()
        .filter(|node| {
            !nodes
                .iter()
                .any(|other| *other != *node && contains_node(def, *other, *node))
        })
        .collect()
}

fn contains_node(def: &BehaviorTreeDef, root: usize, target: usize) -> bool {
    if root == target {
        return true;
    }
    let mut stack = vec![root];
    let mut seen = vec![false; def.nodes.len()];
    while let Some(index) = stack.pop() {
        if index >= def.nodes.len() || seen[index] {
            continue;
        }
        seen[index] = true;
        if index == target {
            return true;
        }
        stack.extend(def.nodes[index].children.iter().copied());
    }
    false
}

/// Static problems of one node, as `(code, message)`. The validator and the compiler share these.
pub(crate) fn static_issues(node: &DefNode) -> Vec<(&'static str, String)> {
    let mut issues = Vec::new();
    let abort_allowed = matches!(
        node.kind,
        DefKind::Condition
            | DefKind::Compare
            | DefKind::Inverter
            | DefKind::Cooldown
            | DefKind::Repeat
            | DefKind::Timeout
            | DefKind::Service
    );
    if node.abort != AbortMode::None && !abort_allowed {
        issues.push(("abort_not_allowed", format!("{:?} cannot abort", node.kind)));
    }
    if node.abort != AbortMode::None && observed_keys(node).is_empty() {
        issues.push(("abort_without_key", "an abort needs a blackboard key".to_string()));
    }
    if node.kind == DefKind::Condition && node.key.is_empty() {
        // Deep-zai lower note: only the validator flagged this; the runtime compile accepted an
        // always-Failure condition. Both share this list.
        issues.push(("empty_key", "a condition needs a blackboard key".to_string()));
    }
    if node.kind == DefKind::Compare {
        if !node.children.is_empty() {
            issues.push(("compare_has_children", "a compare takes no children".to_string()));
        }
        for fault in compare_faults(compare_view(node)) {
            let code = match fault {
                CompareFault::EmptyKey => "empty_key",
                CompareFault::TypeMismatch => "type_mismatch",
                CompareFault::BadCompare => "bad_compare",
            };
            issues.push((code, format!("compare is not valid ({code})")));
        }
    }
    if node.kind == DefKind::StateTree {
        if !node.children.is_empty() {
            issues.push((
                "state_tree_has_children",
                "a state_tree takes no children".to_string(),
            ));
        }
        if node.task.is_empty() {
            issues.push((
                "empty_state_tree",
                "a state_tree needs a .statetree.ron path".to_string(),
            ));
        } else if !node.task.ends_with(".statetree.ron") {
            issues.push((
                "bad_state_tree",
                format!("state tree path {:?} must end with .statetree.ron", node.task),
            ));
        }
    }
    issues
}

/// Rejects a definition `tick` could not run: an empty arena, a root or child index outside the
/// arena, a condition with children, any node reachable twice from the root (a cycle, or a node
/// with two parents), and a path from the root longer than [`MAX_BT_DEPTH`] nodes.
///
/// `base` is the depth already consumed by the trees enclosing this one through `state_tree`
/// bridges (0 at the top level): the cap spans the whole chain, because a bridge task's tick
/// runs the inner tree on top of the outer tree's recursion frames. Returns the tree's height
/// (the longest root-to-leaf path) for the next link of the chain.
fn validate_graph(def: &BehaviorTreeDef, base: usize) -> Result<usize, BtCompileError> {
    let count = def.nodes.len();
    if def.root >= count {
        return Err(BtCompileError::Malformed(format!(
            "root index {} is outside the {count}-node arena",
            def.root
        )));
    }
    for (index, node) in def.nodes.iter().enumerate() {
        if let Some(bad) = node.children.iter().find(|c| **c >= count) {
            return Err(BtCompileError::Malformed(format!(
                "node {index} ({:?}) has child index {bad} outside the {count}-node arena",
                node.kind
            )));
        }
        if node.kind == DefKind::Condition && !node.children.is_empty() {
            return Err(BtCompileError::Malformed(format!(
                "node {index} (Condition) takes no children"
            )));
        }
    }
    let mut seen = vec![false; count];
    let mut stack = vec![(def.root, 1usize)];
    let mut height = 0usize;
    while let Some((index, depth)) = stack.pop() {
        if std::mem::replace(&mut seen[index], true) {
            return Err(BtCompileError::Malformed(format!(
                "node {index} is reachable twice (a cycle, or a node with two parents)"
            )));
        }
        if base + depth > MAX_BT_DEPTH {
            return Err(BtCompileError::Malformed(format!(
                "node {index} is {depth} levels deep in this tree, {base} more above it through \
                 state-tree bridges; a tree, or a chain of trees joined by bridges, may be at \
                 most {MAX_BT_DEPTH} levels deep"
            )));
        }
        height = height.max(depth);
        stack.extend(def.nodes[index].children.iter().map(|child| (*child, depth + 1)));
    }
    // Unreachable nodes are still compiled, so reject a cycle among them too: an iterative
    // three-color DFS over every node. 0 = unvisited, 1 = on the current path, 2 = done.
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
                match color[child] {
                    1 => {
                        return Err(BtCompileError::Malformed(format!(
                            "node {node} ({:?}) is part of a cycle through child {child}",
                            def.nodes[node].name
                        )));
                    }
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
    Ok(height)
}

/// A decorator's `seconds`, rejecting NaN, infinite and negative values.
fn checked_seconds(index: usize, node: &DefNode) -> Result<f32, BtCompileError> {
    if node.seconds.is_finite() && node.seconds >= 0.0 {
        Ok(node.seconds)
    } else {
        Err(BtCompileError::Malformed(format!(
            "node {index} ({:?}) needs a finite, non-negative `seconds` (got {})",
            node.kind, node.seconds
        )))
    }
}

/// Compiles one definition node into a runtime node, validating child indices.
fn compile_def_node(
    index: usize,
    node: &DefNode,
    registry: &BtTaskRegistry,
    node_count: usize,
    load: &dyn Fn(&str) -> Option<String>,
) -> Result<BtNode, BtCompileError> {
    if let Some((code, _)) = static_issues(node).into_iter().next() {
        return Err(BtCompileError::Malformed(format!("node {index}: {code}")));
    }
    let single_child = |children: &[usize]| -> Result<usize, BtCompileError> {
        match children {
            [child] if *child < node_count => Ok(*child),
            _ => Err(BtCompileError::Malformed(format!(
                "node {index} ({:?}) expects exactly one child",
                node.kind
            ))),
        }
    };
    Ok(match node.kind {
        DefKind::Selector => BtNode::Selector(node.children.clone()),
        DefKind::Sequence => BtNode::Sequence(node.children.clone()),
        DefKind::Parallel => BtNode::Parallel {
            children: node.children.clone(),
            policy: node.policy,
        },
        DefKind::Inverter => BtNode::Inverter(single_child(&node.children)?),
        DefKind::Condition => BtNode::Condition(node.key.clone()),
        DefKind::Compare => BtNode::Compare,
        DefKind::StateTree => {
            return compile_state_tree(node, registry, load);
        }
        DefKind::Cooldown => BtNode::Cooldown {
            seconds: checked_seconds(index, node)?,
            child: single_child(&node.children)?,
        },
        DefKind::Repeat => BtNode::Repeat {
            times: node.times,
            child: single_child(&node.children)?,
        },
        DefKind::Timeout => BtNode::Timeout {
            seconds: checked_seconds(index, node)?,
            child: single_child(&node.children)?,
        },
        DefKind::Service => BtNode::Service {
            task: registry
                .construct(&node.task)
                .ok_or_else(|| BtCompileError::UnknownTask(node.name.clone(), node.task.clone()))?,
            child: single_child(&node.children)?,
        },
        DefKind::Leaf => {
            let task = registry
                .construct(&node.task)
                .ok_or_else(|| BtCompileError::UnknownTask(node.name.clone(), node.task.clone()))?;
            BtNode::Leaf(task)
        }
    })
}

fn compile_state_tree(
    node: &DefNode,
    registry: &BtTaskRegistry,
    load: &dyn Fn(&str) -> Option<String>,
) -> Result<BtNode, BtCompileError> {
    let _guard = crate::bt_bridge::enter_bridge(&node.task).map_err(|code| BtCompileError::BadStateTree {
        node: node.name.clone(),
        path: node.task.clone(),
        detail: code.to_string(),
    })?;
    let text = load(&node.task).ok_or_else(|| BtCompileError::BadStateTree {
        node: node.name.clone(),
        path: node.task.clone(),
        detail: "could not load the state tree".to_string(),
    })?;
    let def = StateTreeDef::from_ron(&text).map_err(|err| BtCompileError::BadStateTree {
        node: node.name.clone(),
        path: node.task.clone(),
        detail: err.to_string(),
    })?;
    let tree = StateTree::compile_with(&def, registry, load).map_err(|err| BtCompileError::BadStateTree {
        node: node.name.clone(),
        path: node.task.clone(),
        detail: err.to_string(),
    })?;
    Ok(BtNode::StateTree(Box::new(tree)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blackboard::Blackboard;
    use crate::task::{BtTask, BtTaskRegistration};

    /// A leaf that succeeds after `n` ticks (counts via its own state).
    #[derive(Default)]
    struct SucceedAfter(u32);

    impl BtTask for SucceedAfter {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            if self.0 == 0 {
                BtStatus::Success
            } else {
                self.0 -= 1;
                BtStatus::Running
            }
        }
    }
    impl BtTaskRegistration for SucceedAfter {
        fn construct() -> Box<dyn BtTask> {
            Box::new(SucceedAfter::default())
        }
        fn task_type_path() -> &'static str {
            "test::SucceedAfter"
        }
    }

    fn registry() -> BtTaskRegistry {
        let mut r = BtTaskRegistry::default();
        r.register::<SucceedAfter>();
        r
    }

    fn ctx<'a>(world: &'a mut World, entity: Entity, dt: f32) -> BtContext<'a> {
        BtContext { world, entity, dt }
    }

    #[test]
    fn condition_reads_the_blackboard() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut board = Blackboard::default();
        board.values = [("flag".to_string(), crate::blackboard::BbValue::Bool(true))]
            .into_iter()
            .collect();
        let entity = app.world_mut().spawn(board).id();
        let def = BehaviorTreeDef::condition("c", "flag");
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        let status = tree.tick(&mut ctx(app.world_mut(), entity, 0.1));
        assert_eq!(status, BtStatus::Success);
    }

    #[test]
    fn sequence_short_circuits_on_failure() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let cond = b.condition("false", "missing");
        let leaf = b.leaf("never", "test::SucceedAfter");
        let root = b.sequence("root", vec![cond, leaf]);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Failure
        );
    }

    #[test]
    fn selector_tries_children_until_one_succeeds() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let cond = b.condition("false", "missing");
        let leaf = b.leaf("ok", "test::SucceedAfter");
        let root = b.selector("root", vec![cond, leaf]);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
    }

    #[test]
    fn repeat_counts_successes() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("leaf", "test::SucceedAfter");
        let root = b.repeat("r", Some(3), leaf);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Running
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Running
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
    }

    #[test]
    fn a_service_runs_every_tick_and_returns_the_child_status() {
        // A service task increments a blackboard counter each tick; its child is a leaf.
        #[derive(Default)]
        struct CountService;
        impl BtTask for CountService {
            fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
                if let Some(mut bb) = ctx.blackboard_mut() {
                    let n = bb.get_int("count") + 1;
                    bb.insert("count", n);
                }
                BtStatus::Running
            }
        }
        impl BtTaskRegistration for CountService {
            fn construct() -> Box<dyn BtTask> {
                Box::new(CountService)
            }
            fn task_type_path() -> &'static str {
                "test::CountService"
            }
        }

        let mut registry = BtTaskRegistry::default();
        registry.register::<SucceedAfter>();
        registry.register::<CountService>();

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();

        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("ok", "test::SucceedAfter");
        let root = b.service("svc", "test::CountService", leaf);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &registry).expect("compiles");

        // The service task runs on every tick, and the service returns the child's status.
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
        // The service's child leaf is ticked through `tick_node`, so its per-node status is
        // recorded (not left at the initial `Running`).
        assert_eq!(
            tree.node_statuses()[leaf],
            BtStatus::Success,
            "the service child's status is reflected"
        );
        let bb = app.world().get::<Blackboard>(entity).expect("blackboard");
        assert_eq!(bb.get_int("count"), 2, "the service ran on each tick");
    }

    #[test]
    fn node_statuses_are_recorded_after_each_tick() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let cond = b.condition("false", "missing");
        let leaf = b.leaf("ok", "test::SucceedAfter");
        let root = b.selector("root", vec![cond, leaf]);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        let status = tree.tick(&mut ctx(app.world_mut(), entity, 0.1));
        assert_eq!(status, BtStatus::Success);
        assert_eq!(tree.node_statuses().len(), def.nodes.len(), "one status per node");
        assert_eq!(tree.node_statuses()[def.root], BtStatus::Success);
        // The status snapshot is reflectable and copies the recorded state.
        let snapshot = BehaviorTreeStatus {
            last_status: status,
            node_status: tree.node_statuses().to_vec(),
        };
        assert_eq!(snapshot.last_status, BtStatus::Success);
        assert_eq!(snapshot.node_status, tree.node_statuses());
    }

    #[test]
    fn unknown_task_is_rejected_at_compile_time() {
        let def = BehaviorTreeDef::leaf("bad", "no::such::Task");
        assert!(matches!(
            BehaviorTree::compile(&def, &registry()),
            Err(BtCompileError::UnknownTask(_, _))
        ));
    }

    #[test]
    fn compile_rejects_empty_out_of_range_and_cyclic_arenas() {
        let reg = registry();
        let empty = BehaviorTreeDef::default();
        assert!(matches!(
            BehaviorTree::compile(&empty, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        let bad_child = BehaviorTreeDef::selector("root", vec![7]);
        assert!(matches!(
            BehaviorTree::compile(&bad_child, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        let self_cycle = BehaviorTreeDef::selector("root", vec![0]);
        assert!(matches!(
            BehaviorTree::compile(&self_cycle, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.condition("c", "k");
        let seq = b.sequence("s", vec![leaf]);
        let mut def = b.build(seq);
        def.nodes[leaf].children.push(seq);
        assert!(matches!(
            BehaviorTree::compile(&def, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        let mut bad_root = def.clone();
        bad_root.nodes[leaf].children.clear();
        bad_root.root = 99;
        assert!(matches!(
            BehaviorTree::compile(&bad_root, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        def.nodes[leaf].children.clear();
        assert!(BehaviorTree::compile(&def, &reg).is_ok());
    }

    #[test]
    fn def_is_reflect_and_serde_round_trips() {
        let mut b = BehaviorTreeBuilder::new();
        let cond = b.condition("alive", "alive");
        let root = b.selector("root", vec![cond]);
        let def = b.build(root);
        let ron = ron::to_string(&def).expect("serializes");
        let back: BehaviorTreeDef = ron::from_str(&ron).expect("deserializes");
        assert_eq!(def, back);
    }

    /// Adds one to the agent's `ticks` blackboard counter.
    fn bump_ticks(ctx: &mut BtContext<'_>) {
        if let Some(mut bb) = ctx.blackboard_mut() {
            let n = bb.get_int("ticks") + 1;
            bb.insert("ticks", n);
        }
    }

    /// A leaf that always succeeds and counts its ticks.
    #[derive(Default)]
    struct Win;
    impl BtTask for Win {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            bump_ticks(ctx);
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for Win {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Win)
        }
        fn task_type_path() -> &'static str {
            "test::Win"
        }
    }

    /// A leaf that always fails and counts its ticks.
    #[derive(Default)]
    struct Lose;
    impl BtTask for Lose {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            bump_ticks(ctx);
            BtStatus::Failure
        }
    }
    impl BtTaskRegistration for Lose {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Lose)
        }
        fn task_type_path() -> &'static str {
            "test::Lose"
        }
    }

    /// A leaf that never finishes and counts its ticks.
    #[derive(Default)]
    struct Busy;
    impl BtTask for Busy {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            bump_ticks(ctx);
            BtStatus::Running
        }
    }
    impl BtTaskRegistration for Busy {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Busy)
        }
        fn task_type_path() -> &'static str {
            "test::Busy"
        }
    }

    fn fixed_registry() -> BtTaskRegistry {
        let mut r = BtTaskRegistry::default();
        r.register::<Win>();
        r.register::<Lose>();
        r.register::<Busy>();
        r
    }

    /// A world with one agent that carries an empty blackboard.
    fn agent_world() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        (app, entity)
    }

    fn ticks(app: &App, entity: Entity) -> i64 {
        app.world()
            .get::<Blackboard>(entity)
            .expect("blackboard")
            .get_int("ticks")
    }

    /// Builds a parallel of the named fixed leaves and ticks it once.
    fn parallel_once(policy: ParallelPolicy, leaves: &[&str]) -> (BtStatus, i64) {
        let (mut app, entity) = agent_world();
        let mut b = BehaviorTreeBuilder::new();
        let children: Vec<usize> = leaves
            .iter()
            .map(|task| b.leaf(*task, format!("test::{task}")))
            .collect();
        let root = b.parallel("par", policy, children);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &fixed_registry()).expect("compiles");
        let status = tree.tick(&mut ctx(app.world_mut(), entity, 0.1));
        (status, ticks(&app, entity))
    }

    #[test]
    fn parallel_require_all_needs_every_child_to_succeed() {
        let (status, n) = parallel_once(ParallelPolicy::RequireAll, &["Win", "Win", "Win"]);
        assert_eq!(status, BtStatus::Success);
        assert_eq!(n, 3, "every child is ticked");
        // A running child keeps the whole node running.
        let (status, n) = parallel_once(ParallelPolicy::RequireAll, &["Win", "Busy"]);
        assert_eq!(status, BtStatus::Running);
        assert_eq!(n, 2);
        // Any failure fails the node, and the later children are still ticked.
        let (status, n) = parallel_once(ParallelPolicy::RequireAll, &["Lose", "Busy", "Win"]);
        assert_eq!(status, BtStatus::Failure);
        assert_eq!(n, 3);
    }

    #[test]
    fn parallel_require_one_succeeds_on_any_success_and_fails_only_when_all_fail() {
        let (status, n) = parallel_once(ParallelPolicy::RequireOne, &["Lose", "Win", "Busy"]);
        assert_eq!(status, BtStatus::Success);
        assert_eq!(n, 3, "every child is ticked");
        let (status, _) = parallel_once(ParallelPolicy::RequireOne, &["Lose", "Busy"]);
        assert_eq!(status, BtStatus::Running);
        let (status, _) = parallel_once(ParallelPolicy::RequireOne, &["Busy"]);
        assert_eq!(status, BtStatus::Running);
        let (status, _) = parallel_once(ParallelPolicy::RequireOne, &["Lose", "Lose"]);
        assert_eq!(status, BtStatus::Failure);
    }

    #[test]
    fn inverter_flips_success_and_failure_and_keeps_running() {
        for (leaf, expected) in [
            ("Win", BtStatus::Failure),
            ("Lose", BtStatus::Success),
            ("Busy", BtStatus::Running),
        ] {
            let (mut app, entity) = agent_world();
            let mut b = BehaviorTreeBuilder::new();
            let child = b.leaf(leaf, format!("test::{leaf}"));
            let root = b.inverter("inv", child);
            let def = b.build(root);
            let mut tree = BehaviorTree::compile(&def, &fixed_registry()).expect("compiles");
            assert_eq!(
                tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
                expected,
                "inverter over {leaf}"
            );
        }
    }

    #[test]
    fn cooldown_blocks_the_child_until_the_time_has_passed() {
        let (mut app, entity) = agent_world();
        let mut b = BehaviorTreeBuilder::new();
        let child = b.leaf("win", "test::Win");
        let root = b.cooldown("cd", 1.0, child);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &fixed_registry()).expect("compiles");

        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
        assert_eq!(ticks(&app, entity), 1);
        // Inside the cooldown the node fails and the child is not ticked.
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Failure
        );
        assert_eq!(ticks(&app, entity), 1);
        // The tick whose dt crosses zero ends the cooldown and ticks the child (ADR-1059).
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 5.0)),
            BtStatus::Success
        );
        assert_eq!(ticks(&app, entity), 2);
        // That success restarted the cooldown.
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Failure
        );
        assert_eq!(ticks(&app, entity), 2);
    }

    #[test]
    fn timeout_ticks_the_child_then_fails_once_the_budget_is_spent() {
        let (mut app, entity) = agent_world();
        let mut b = BehaviorTreeBuilder::new();
        let child = b.leaf("busy", "test::Busy");
        let root = b.timeout("to", 1.0, child);
        let def = b.build(root);
        let mut tree = BehaviorTree::compile(&def, &fixed_registry()).expect("compiles");

        // A first tick that spends the whole budget runs the child once, then fails (ADR-1059).
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 1.0)),
            BtStatus::Failure
        );
        assert_eq!(ticks(&app, entity), 1);
        // The failure reset the budget: two short ticks run the child.
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.4)),
            BtStatus::Running
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.4)),
            BtStatus::Running
        );
        assert_eq!(ticks(&app, entity), 3);
        // The third short tick runs the child and crosses the budget.
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.4)),
            BtStatus::Failure
        );
        assert_eq!(ticks(&app, entity), 4);
    }

    #[test]
    fn timeout_clears_its_clock_when_the_child_finishes() {
        for leaf in ["Win", "Lose"] {
            let (mut app, entity) = agent_world();
            let mut b = BehaviorTreeBuilder::new();
            let child = b.leaf(leaf, format!("test::{leaf}"));
            let root = b.timeout("to", 1.0, child);
            let def = b.build(root);
            let mut tree = BehaviorTree::compile(&def, &fixed_registry()).expect("compiles");
            let first = tree.tick(&mut ctx(app.world_mut(), entity, 0.6));
            assert_ne!(first, BtStatus::Running);
            assert!(
                tree.state[root].timeout_elapsed.abs() < f64::EPSILON,
                "{leaf}: the clock restarts after a finished child"
            );
            // A full budget is available again, so the child is ticked rather than timed out.
            let second = tree.tick(&mut ctx(app.world_mut(), entity, 0.6));
            assert_eq!(second, first, "{leaf}");
            assert_eq!(ticks(&app, entity), 2, "{leaf}");
        }
    }

    /// Bumps the blackboard counter `key` on the agent.
    fn bump(ctx: &mut BtContext<'_>, key: &str) {
        if let Some(mut bb) = ctx.blackboard_mut() {
            let n = bb.get_int(key) + 1;
            bb.insert(key, n);
        }
    }

    /// Succeeds on its first tick; counts its ticks in `fires`.
    #[derive(Default)]
    struct FireOnce;
    impl BtTask for FireOnce {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            bump(ctx, "fires");
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for FireOnce {
        fn construct() -> Box<dyn BtTask> {
            Box::new(FireOnce)
        }
        fn task_type_path() -> &'static str {
            "test::FireOnce"
        }
    }

    /// Runs for `TWO` ticks then succeeds; counts `reset` calls in `resets` and restarts its count.
    #[derive(Default)]
    struct RunTwo(u32);
    impl BtTask for RunTwo {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            self.0 += 1;
            if self.0 > 2 {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
        fn reset(&mut self, ctx: &mut BtContext<'_>) {
            self.0 = 0;
            bump(ctx, "resets");
        }
    }
    impl BtTaskRegistration for RunTwo {
        fn construct() -> Box<dyn BtTask> {
            Box::new(RunTwo(0))
        }
        fn task_type_path() -> &'static str {
            "test::RunTwo"
        }
    }

    fn full_registry() -> BtTaskRegistry {
        let mut r = registry();
        r.register::<FireOnce>();
        r.register::<RunTwo>();
        r
    }

    fn agent() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        (app, entity)
    }

    fn count(app: &App, entity: Entity, key: &str) -> i64 {
        app.world()
            .get::<Blackboard>(entity)
            .expect("blackboard")
            .get_int(key)
    }

    #[test]
    fn parallel_does_not_retick_a_finished_child() {
        let (mut app, entity) = agent();
        let mut b = BehaviorTreeBuilder::new();
        let fire = b.leaf("fire", "test::FireOnce");
        let run = b.leaf("run", "test::RunTwo");
        let root = b.parallel("par", ParallelPolicy::RequireAll, vec![fire, run]);
        let mut tree = BehaviorTree::compile(&b.build(root), &full_registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Running
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Running
        );
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
        assert_eq!(
            count(&app, entity, "fires"),
            1,
            "the finished child fired exactly once"
        );
    }

    #[test]
    fn a_finished_parallel_halts_the_running_siblings() {
        let (mut app, entity) = agent();
        let mut b = BehaviorTreeBuilder::new();
        let run = b.leaf("run", "test::RunTwo");
        let fire = b.leaf("fire", "test::FireOnce");
        let root = b.parallel("par", ParallelPolicy::RequireOne, vec![run, fire]);
        let mut tree = BehaviorTree::compile(&b.build(root), &full_registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.1)),
            BtStatus::Success
        );
        assert!(
            count(&app, entity, "resets") >= 1,
            "the running sibling was reset"
        );
    }

    #[test]
    fn repeat_resets_its_leaf_between_iterations() {
        let (mut app, entity) = agent();
        let mut b = BehaviorTreeBuilder::new();
        let run = b.leaf("run", "test::RunTwo");
        let root = b.repeat("rep", Some(2), run);
        let mut tree = BehaviorTree::compile(&b.build(root), &full_registry()).expect("compiles");
        // Each iteration takes three ticks (two Running, then Success) because the leaf restarts.
        let mut statuses = Vec::new();
        for _ in 0..6 {
            statuses.push(tree.tick(&mut ctx(app.world_mut(), entity, 0.1)));
        }
        assert_eq!(
            statuses,
            vec![
                BtStatus::Running,
                BtStatus::Running,
                BtStatus::Running,
                BtStatus::Running,
                BtStatus::Running,
                BtStatus::Success
            ]
        );
    }

    #[test]
    fn compile_rejects_graphs_tick_cannot_run() {
        let reg = full_registry();
        // Empty arena (also what `BehaviorTreeDef::default()` is).
        assert!(matches!(
            BehaviorTree::compile(&BehaviorTreeDef::default(), &reg),
            Err(BtCompileError::Malformed(_))
        ));
        // A selector child outside the arena.
        let out_of_range = BehaviorTreeDef::selector("s", vec![99]);
        assert!(matches!(
            BehaviorTree::compile(&out_of_range, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        // A root outside the arena.
        let mut bad_root = BehaviorTreeDef::leaf("l", "test::FireOnce");
        bad_root.root = 5;
        assert!(matches!(
            BehaviorTree::compile(&bad_root, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        // An inverter whose child is itself.
        let cycle = BehaviorTreeDef::decorated(DefKind::Inverter, "i", 0);
        assert!(matches!(
            BehaviorTree::compile(&cycle, &reg),
            Err(BtCompileError::Malformed(_))
        ));
        // A node with two parents.
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("l", "test::FireOnce");
        let root = b.sequence("s", vec![leaf, leaf]);
        assert!(matches!(
            BehaviorTree::compile(&b.build(root), &reg),
            Err(BtCompileError::Malformed(_))
        ));
        // A condition with a child.
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("l", "test::FireOnce");
        let cond = b.add(DefNode {
            name: "c".into(),
            kind: DefKind::Condition,
            children: vec![leaf],
            ..Default::default()
        });
        assert!(matches!(
            BehaviorTree::compile(&b.build(cond), &reg),
            Err(BtCompileError::Malformed(_))
        ));
    }

    #[test]
    fn compile_rejects_out_of_range_children_root_and_cycles() {
        let leaf = |name: &str| DefNode {
            name: name.to_string(),
            kind: DefKind::Leaf,
            task: "test::SucceedAfter".to_string(),
            ..DefNode::default()
        };
        let composite = |kind: DefKind, children: Vec<usize>| DefNode {
            kind,
            children,
            ..DefNode::default()
        };
        let malformed = |def: BehaviorTreeDef| {
            matches!(
                BehaviorTree::compile(&def, &registry()),
                Err(BtCompileError::Malformed(_))
            )
        };
        // A selector child past the arena.
        assert!(malformed(BehaviorTreeDef {
            root: 0,
            nodes: vec![composite(DefKind::Selector, vec![99]), leaf("a")],
            ..BehaviorTreeDef::default()
        }));
        // A root past the arena, and a root on an empty arena.
        assert!(malformed(BehaviorTreeDef {
            root: 5,
            nodes: vec![leaf("a")],
            ..BehaviorTreeDef::default()
        }));
        assert!(malformed(BehaviorTreeDef::default()));
        // A sequence that contains itself, and a two-node cycle.
        assert!(malformed(BehaviorTreeDef {
            root: 0,
            nodes: vec![composite(DefKind::Sequence, vec![0])],
            ..BehaviorTreeDef::default()
        }));
        assert!(malformed(BehaviorTreeDef {
            root: 0,
            nodes: vec![
                composite(DefKind::Selector, vec![1]),
                composite(DefKind::Sequence, vec![0]),
            ],
            ..BehaviorTreeDef::default()
        }));
        // A shared child (a DAG, not a cycle) is rejected too: node state is per node, so two
        // parents would share one task instance and its latches (ADR-1315).
        assert!(malformed(BehaviorTreeDef {
            root: 0,
            nodes: vec![
                composite(DefKind::Selector, vec![1, 2]),
                composite(DefKind::Sequence, vec![3]),
                composite(DefKind::Sequence, vec![3]),
                leaf("a"),
            ],
            ..BehaviorTreeDef::default()
        }));
        // The same shape with one parent per node compiles.
        let ok = BehaviorTreeDef {
            root: 0,
            nodes: vec![
                composite(DefKind::Selector, vec![1, 2]),
                composite(DefKind::Sequence, vec![3]),
                composite(DefKind::Sequence, vec![4]),
                leaf("a"),
                leaf("b"),
            ],
            ..BehaviorTreeDef::default()
        };
        assert!(BehaviorTree::compile(&ok, &registry()).is_ok());
    }

    /// A leaf that counts its ticks in the blackboard key `ticks` and returns `status`.
    #[derive(Default)]
    struct Counter {
        succeed: bool,
    }

    impl BtTask for Counter {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            if let Some(mut bb) = ctx.blackboard_mut() {
                let n = bb.get_int("ticks");
                bb.insert("ticks", n + 1);
            }
            if self.succeed {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
    }
    struct Spin;
    impl BtTaskRegistration for Spin {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Counter { succeed: false })
        }
        fn task_type_path() -> &'static str {
            "test::Spin"
        }
    }
    struct Hit;
    impl BtTaskRegistration for Hit {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Counter { succeed: true })
        }
        fn task_type_path() -> &'static str {
            "test::Hit"
        }
    }

    fn counting_registry() -> BtTaskRegistry {
        let mut r = BtTaskRegistry::default();
        r.register::<Spin>();
        r.register::<Hit>();
        r
    }

    /// Ticks `tree` `steps` times at `dt`, returning every status.
    fn run(tree: &mut BehaviorTree, app: &mut App, entity: Entity, dt: f32, steps: usize) -> Vec<BtStatus> {
        (0..steps)
            .map(|_| tree.tick(&mut ctx(app.world_mut(), entity, dt)))
            .collect()
    }

    #[test]
    fn timeout_runs_the_child_once_per_budgeted_step() {
        // dt 0.25 is exact in binary: a 0.75 s budget is exactly 3 steps.
        for (seconds, expected) in [(0.25f32, 1i64), (0.75, 3)] {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins);
            let entity = app.world_mut().spawn(Blackboard::default()).id();
            let mut b = BehaviorTreeBuilder::new();
            let leaf = b.leaf("spin", "test::Spin");
            let root = b.timeout("t", seconds, leaf);
            let mut tree = BehaviorTree::compile(&b.build(root), &counting_registry()).expect("compiles");
            let statuses = run(&mut tree, &mut app, entity, 0.25, expected as usize);
            assert_eq!(statuses.last(), Some(&BtStatus::Failure), "budget {seconds}");
            assert_eq!(ticks(&app, entity), expected, "budget {seconds}");
        }
    }

    #[test]
    fn cooldown_blocks_exactly_its_duration() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("hit", "test::Hit");
        let root = b.cooldown("c", 0.5, leaf);
        let mut tree = BehaviorTree::compile(&b.build(root), &counting_registry()).expect("compiles");
        // Success, then two blocked steps (0.5 s = 2 x 0.25 s), then the child runs again.
        let statuses = run(&mut tree, &mut app, entity, 0.25, 4);
        assert_eq!(
            statuses,
            [
                BtStatus::Success,
                BtStatus::Failure,
                BtStatus::Success,
                BtStatus::Failure
            ]
        );
        assert_eq!(ticks(&app, entity), 2);
    }

    #[test]
    fn a_long_running_timeout_does_not_lose_steps_to_f32_rounding() {
        // At 1/60 s the f32 accumulator drifts after a few minutes; the f64 one must not.
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("spin", "test::Spin");
        let root = b.timeout("t", 600.0, leaf);
        let mut tree = BehaviorTree::compile(&b.build(root), &counting_registry()).expect("compiles");
        let dt = 1.0f32 / 60.0;
        let mut failed_at = None;
        for step in 1..=40_000usize {
            if tree.tick(&mut ctx(app.world_mut(), entity, dt)) == BtStatus::Failure {
                failed_at = Some(step);
                break;
            }
        }
        // 600 s / (1/60 s) = 36000 steps (+/- one step for the f32 dt value).
        let step = failed_at.expect("timeout fires");
        assert!((35_999..=36_001).contains(&step), "fired at step {step}");
    }

    #[test]
    fn repeat_zero_does_not_tick_its_child() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn(Blackboard::default()).id();
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("hit", "test::Hit");
        let root = b.repeat("r", Some(0), leaf);
        let mut tree = BehaviorTree::compile(&b.build(root), &counting_registry()).expect("compiles");
        assert_eq!(
            tree.tick(&mut ctx(app.world_mut(), entity, 0.25)),
            BtStatus::Success
        );
        assert_eq!(ticks(&app, entity), 0);
    }

    #[test]
    fn non_finite_or_negative_decorator_seconds_are_rejected() {
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            for timeout in [false, true] {
                let mut b = BehaviorTreeBuilder::new();
                let leaf = b.leaf("hit", "test::Hit");
                let root = if timeout {
                    b.timeout("t", bad, leaf)
                } else {
                    b.cooldown("c", bad, leaf)
                };
                let result = BehaviorTree::compile(&b.build(root), &counting_registry());
                assert!(
                    matches!(result, Err(BtCompileError::Malformed(_))),
                    "{bad} timeout={timeout}"
                );
            }
        }
    }

    /// Deep-zai lower note (`tree.rs` compile): a `Condition` with an empty key always fails,
    /// and only `bt_edit::validate` flagged it (`empty_key`). The runtime compile refuses it
    /// with the same documented code.
    #[test]
    fn a_condition_with_an_empty_key_is_refused_at_compile() {
        let def = BehaviorTreeDef::condition("c", "");
        match BehaviorTree::compile(&def, &registry()) {
            Err(BtCompileError::Malformed(message)) => {
                assert!(message.contains("empty_key"), "{message}");
            }
            Ok(_) => panic!("an always-Failure condition with no key compiled"),
            Err(other) => panic!("expected empty_key, got {other:?}"),
        }
    }
}
