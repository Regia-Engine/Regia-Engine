// SPDX-License-Identifier: MPL-2.0
//! The authored state-tree definition (`.statetree.ron`, format version 2): states, enter
//! conditions, blackboard writes on entry, tasks, prioritized (and optionally held) transitions,
//! child selection, ambient tasks and inclusion of other trees, plus its RON form. Version 1
//! files still load (v2 is a superset).
//!
//! The definition refers to states by name (children, transition targets, the root), so hand
//! authored files stay readable; [`crate::state_tree::StateTree::compile`] resolves the names to
//! indices and checks the whole document.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blackboard::{BbValue, Blackboard};

/// Current `.statetree.ron` format version (spec 38, ADR-1639: v2 adds `enter_set`, `hold`, `fallback`,
/// `ambient`, `includes` and the `Parent` / `Stay` / `NextEnterable` targets).
pub const STATE_TREE_FORMAT_VERSION: u32 = 2;

/// The oldest `.statetree.ron` version this build still reads (through the in-memory migration).
pub const STATE_TREE_MIN_FORMAT_VERSION: u32 = 1;

/// How deep `includes` may nest (a tree including a tree including a tree...).
pub const MAX_INCLUDE_DEPTH: usize = 4;

/// A literal a state writes to the blackboard when it becomes active (`StateDef::enter_set`).
/// Vector and entity values are not authorable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// A 32-bit float (must be finite).
    Float(f32),
    /// A string.
    String(String),
}

impl Literal {
    /// The blackboard value this literal writes.
    pub fn to_value(&self) -> BbValue {
        match self {
            Self::Bool(v) => BbValue::Bool(*v),
            Self::Int(v) => BbValue::Int(*v),
            Self::Float(v) => BbValue::Float(*v),
            Self::String(v) => BbValue::String(v.clone()),
        }
    }
}

/// What a state does when a transition into it finds nothing to enter below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fallback {
    /// The transition is rejected (v1 behavior).
    #[default]
    Stay,
    /// The following siblings are tried in declaration order instead.
    NextEnterable,
}

/// One tree included into another at compile time (`StateTreeDef::includes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncludeDef {
    /// Prefix added (as `prefix/`) to every state name of the included tree.
    pub prefix: String,
    /// Project-relative path of the included `.statetree.ron`, read through the host's loader.
    pub path: String,
}

/// The deepest nesting of `Not` / `All` / `Any` a [`Condition`] may have.
pub const MAX_CONDITION_DEPTH: usize = 16;

/// The most states one definition may declare.
pub const MAX_STATES: usize = 4096;

/// How a [`Condition::Compare`] relates the blackboard value to the constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    /// `value < constant`.
    Lt,
    /// `value <= constant`.
    Le,
    /// `value == constant`.
    Eq,
    /// `value != constant`.
    Ne,
    /// `value >= constant`.
    Ge,
    /// `value > constant`.
    Gt,
}

impl CompareOp {
    fn apply(self, value: f32, constant: f32) -> bool {
        match self {
            Self::Lt => value < constant,
            Self::Le => value <= constant,
            Self::Eq => value == constant,
            Self::Ne => value != constant,
            Self::Ge => value >= constant,
            Self::Gt => value > constant,
        }
    }
}

/// A boolean test over the agent's [`Blackboard`], used for state enter conditions and
/// transition guards. A missing key or a value of the wrong type makes a leaf test `false`
/// (`Not` of it is `true`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    /// Always true.
    #[default]
    Always,
    /// The key holds the boolean `true`.
    Key(String),
    /// The key is present, whatever its type.
    Has(String),
    /// The key holds a number (read losslessly, see [`Blackboard::try_get_float`]) that satisfies
    /// `op` against `value`.
    Compare {
        /// Blackboard key.
        key: String,
        /// Relation.
        op: CompareOp,
        /// Constant to compare against (must be finite).
        value: f32,
    },
    /// The key holds exactly this string.
    Text {
        /// Blackboard key.
        key: String,
        /// Expected string.
        value: String,
    },
    /// Negation.
    Not(Box<Condition>),
    /// Every inner condition holds (an empty list is true).
    All(Vec<Condition>),
    /// At least one inner condition holds (an empty list is false).
    Any(Vec<Condition>),
}

impl Condition {
    /// Evaluates the condition against `bb`.
    pub fn eval(&self, bb: &Blackboard) -> bool {
        match self {
            Self::Always => true,
            Self::Key(key) => bb.get_bool(key),
            Self::Has(key) => bb.contains(key),
            Self::Compare { key, op, value } => bb.try_get_float(key).is_some_and(|v| op.apply(v, *value)),
            Self::Text { key, value } => matches!(bb.get(key), Some(crate::BbValue::String(s)) if s == value),
            Self::Not(inner) => !inner.eval(bb),
            Self::All(items) => items.iter().all(|c| c.eval(bb)),
            Self::Any(items) => items.iter().any(|c| c.eval(bb)),
        }
    }

    /// The nesting depth: a leaf test is 1.
    pub fn depth(&self) -> usize {
        match self {
            Self::Not(inner) => 1 + inner.depth(),
            Self::All(items) | Self::Any(items) => 1 + items.iter().map(Self::depth).max().unwrap_or(0),
            _ => 1,
        }
    }

    /// Whether every numeric constant in the condition is finite.
    pub(crate) fn is_finite(&self) -> bool {
        match self {
            Self::Compare { value, .. } => value.is_finite(),
            Self::Not(inner) => inner.is_finite(),
            Self::All(items) | Self::Any(items) => items.iter().all(Self::is_finite),
            _ => true,
        }
    }
}

/// How a parent state picks which child to enter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selection {
    /// The first child (declaration order) whose enter condition holds.
    #[default]
    First,
    /// The enterable child with the highest utility score; ties go to the earlier child.
    Utility,
    /// A weighted random pick among the enterable children, drawn from the tree's seeded
    /// generator (deterministic for a given seed and history).
    Weighted,
}

/// A utility score read from the blackboard: `bias + scale * value(key)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UtilityDef {
    /// Blackboard key holding the input number (a missing key reads as 0).
    pub key: String,
    /// Multiplier (default 1).
    #[serde(default = "one")]
    pub scale: f32,
    /// Offset added after scaling (default 0).
    #[serde(default)]
    pub bias: f32,
}

/// What makes a transition a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// Considered on every tick (guarded by its condition).
    Tick,
    /// Considered when the named event is delivered.
    Event(String),
    /// Considered while its state's tasks have all succeeded.
    Succeeded,
    /// Considered while one of its state's tasks has failed.
    Failed,
    /// Considered while its state's tasks have all succeeded or one of them has failed (a failure
    /// completes the state at once, even while a sibling task is still running).
    Completed,
}

/// Where a transition goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// The named state (and, below it, whatever its selection picks).
    State(String),
    /// The next sibling of the state that owns the transition.
    Next,
    /// The first later sibling whose enter condition and descent succeed.
    NextEnterable,
    /// The parent of the state that owns the transition (re-entered, then descended by its
    /// selection). Compiling it on the root is an error.
    Parent,
    /// Match and consume the trigger but change nothing (swallows an event).
    Stay,
    /// Finish the whole tree as succeeded.
    Succeed,
    /// Finish the whole tree as failed.
    Fail,
}

/// One transition of a state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionDef {
    /// What triggers it.
    pub trigger: Trigger,
    /// A guard evaluated on the blackboard (default: always true).
    #[serde(default)]
    pub condition: Condition,
    /// Higher wins among the transitions that match in one tick (default 0). Ties go to the
    /// deeper state, then to declaration order.
    #[serde(default)]
    pub priority: i32,
    /// Where it goes.
    pub target: Target,
    /// Seconds the trigger and condition must hold continuously before the transition can fire,
    /// counted in fixed steps (default 0). Not allowed on `Event` triggers.
    #[serde(default)]
    pub hold: f32,
}

/// One state of the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateDef {
    /// Unique name.
    pub name: String,
    /// Child state names, in selection order.
    #[serde(default)]
    pub children: Vec<String>,
    /// How this state picks a child when entered.
    #[serde(default)]
    pub selection: Selection,
    /// The state can only be entered while this holds.
    #[serde(default)]
    pub enter: Condition,
    /// Task type paths (registered [`crate::BtTask`] types) that run while the state is active.
    #[serde(default)]
    pub tasks: Vec<String>,
    /// Transitions out of this state (or its descendants).
    #[serde(default)]
    pub transitions: Vec<TransitionDef>,
    /// Weight when the parent uses [`Selection::Weighted`] (default 1; 0 or less is never picked).
    #[serde(default = "one")]
    pub weight: f32,
    /// Score when the parent uses [`Selection::Utility`] (none scores 0).
    #[serde(default)]
    pub utility: Option<UtilityDef>,
    /// Literals written to the blackboard, in key order, when the state becomes active (after
    /// `enter` passes, before its tasks tick).
    #[serde(default)]
    pub enter_set: BTreeMap<String, Literal>,
    /// What to do when a transition into this state finds nothing to enter below it.
    #[serde(default)]
    pub fallback: Fallback,
}

impl StateDef {
    /// A state with only a name: no children, tasks or transitions.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            children: Vec::new(),
            selection: Selection::First,
            enter: Condition::Always,
            tasks: Vec::new(),
            transitions: Vec::new(),
            weight: 1.0,
            utility: None,
            enter_set: BTreeMap::new(),
            fallback: Fallback::Stay,
        }
    }
}

fn one() -> f32 {
    1.0
}

/// The `.statetree.ron` document.
///
/// ```ron
/// (
///     format_version: 2,
///     root: "Root",
///     states: [
///         (name: "Root", children: ["Idle", "Alert"]),
///         (name: "Idle", transitions: [
///             (trigger: Event("heard"), target: State("Alert")),
///         ]),
///         (name: "Alert", tasks: ["game::Investigate"], transitions: [
///             (trigger: Succeeded, target: State("Idle")),
///         ]),
///     ],
/// )
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTreeDef {
    /// Format version, [`STATE_TREE_FORMAT_VERSION`] (a version 1 file reads as version 2).
    pub format_version: u32,
    /// What the tree is for.
    #[serde(default)]
    pub description: String,
    /// Name of the root state.
    pub root: String,
    /// Seed of the generator used by [`Selection::Weighted`].
    #[serde(default)]
    pub seed: u64,
    /// When non-empty, the name of the active leaf state is written to this blackboard string
    /// key every time the active path changes (the empty string once the tree finishes or
    /// restarts).
    #[serde(default)]
    pub state_key: String,
    /// Task paths ticked first on every tick while the tree runs (their results are ignored).
    #[serde(default)]
    pub ambient: Vec<String>,
    /// Trees inlined into this one at compile time.
    #[serde(default)]
    pub includes: Vec<IncludeDef>,
    /// The states, in any order.
    pub states: Vec<StateDef>,
}

/// Why a definition could not be read or compiled.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StateTreeError {
    /// The RON text did not parse.
    #[error("state tree RON is invalid: {0}")]
    Parse(String),
    /// The RON text could not be written.
    #[error("state tree could not be written as RON: {0}")]
    Write(String),
    /// The file's `format_version` is not one this build reads.
    #[error("state tree format_version {found} is not supported (this build reads version {supported})")]
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },
    /// The definition has no states.
    #[error("state tree has no states")]
    Empty,
    /// More than [`MAX_STATES`] states.
    #[error("state tree has {0} states (the limit is {MAX_STATES})")]
    TooManyStates(usize),
    /// A state has an empty name.
    #[error("state tree has a state with an empty name")]
    EmptyName,
    /// Two states share a name.
    #[error("state tree declares the state {0:?} twice")]
    DuplicateName(String),
    /// The root, a child or a transition target names no state.
    #[error("state tree refers to the unknown state {name:?} (from {from:?})")]
    UnknownState {
        /// The missing name.
        name: String,
        /// Where it was referred to.
        from: String,
    },
    /// A state is listed as a child of more than one parent (or twice), or the root is a child.
    #[error("state {0:?} has more than one parent")]
    MultipleParents(String),
    /// A state cannot be reached from the root.
    #[error("state {0:?} is not reachable from the root")]
    Unreachable(String),
    /// A task type path is not registered.
    #[error("state {state:?} references unregistered task type {task:?}")]
    UnknownTask {
        /// The state.
        state: String,
        /// The task type path.
        task: String,
    },
    /// A `Next` target on a state with no next sibling.
    #[error("state {0:?} has a Next transition but no next sibling")]
    NoNextSibling(String),
    /// A condition nests deeper than [`MAX_CONDITION_DEPTH`].
    #[error("state {0:?} has a condition nested deeper than {MAX_CONDITION_DEPTH}")]
    ConditionTooDeep(String),
    /// A weight, scale, bias or comparison constant is NaN or infinite, or an event name is empty.
    #[error("state {0:?} has a non-finite number or an empty event name")]
    BadValue(String),
    /// An included file could not be read.
    #[error("state tree include {0:?} could not be loaded")]
    IncludeMissing(String),
    /// A tree includes itself, directly or through other includes.
    #[error("state tree include {0:?} is part of an include cycle")]
    IncludeCycle(String),
    /// Includes nest deeper than [`MAX_INCLUDE_DEPTH`].
    #[error("state tree includes nest deeper than {MAX_INCLUDE_DEPTH}")]
    IncludeDepth,
    /// A transition with an `Event` trigger declares a `hold`.
    #[error("state {0:?} has an Event transition with a hold")]
    HoldOnEvent(String),
    /// A `Parent` target on the root state.
    #[error("state {0:?} is the root and has a Parent transition")]
    ParentOfRoot(String),
    /// A `bt:` task could not be loaded or compiled (ADR-4543).
    #[error("state {state:?} could not run behavior tree: {detail}")]
    BadBehaviorTree {
        /// The state, or `<ambient>`.
        state: String,
        /// Why the behavior tree failed.
        detail: String,
    },
}

impl StateTreeError {
    /// The snake_case reason used on the agent protocol (`data.reason`).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Parse(_) | Self::Write(_) => "parse",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Empty => "empty",
            Self::TooManyStates(_) => "too_many_states",
            Self::EmptyName => "empty_name",
            Self::DuplicateName(_) => "duplicate_name",
            Self::UnknownState { .. } => "unknown_state",
            Self::MultipleParents(_) => "multiple_parents",
            Self::Unreachable(_) => "unreachable",
            Self::UnknownTask { .. } => "unknown_task",
            Self::NoNextSibling(_) => "no_next_sibling",
            Self::ConditionTooDeep(_) => "condition_too_deep",
            Self::BadValue(_) => "bad_value",
            Self::IncludeMissing(_) => "include_missing",
            Self::IncludeCycle(_) => "include_cycle",
            Self::IncludeDepth => "include_depth",
            Self::HoldOnEvent(_) => "hold_on_event",
            Self::ParentOfRoot(_) => "parent_of_root",
            Self::BadBehaviorTree { .. } => "bad_behavior_tree",
        }
    }

    /// The state the error is about, when it names one.
    pub fn state(&self) -> Option<&str> {
        match self {
            Self::DuplicateName(s)
            | Self::MultipleParents(s)
            | Self::Unreachable(s)
            | Self::NoNextSibling(s)
            | Self::ConditionTooDeep(s)
            | Self::BadValue(s)
            | Self::HoldOnEvent(s)
            | Self::ParentOfRoot(s) => Some(s.as_str()),
            Self::UnknownState { from, .. } => Some(from.as_str()),
            Self::UnknownTask { state, .. } | Self::BadBehaviorTree { state, .. } => Some(state.as_str()),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct VersionHeader {
    format_version: u32,
}

impl StateTreeDef {
    /// An empty definition with the given root name (no states yet).
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            format_version: STATE_TREE_FORMAT_VERSION,
            description: String::new(),
            root: root.into(),
            seed: 0,
            state_key: String::new(),
            ambient: Vec::new(),
            includes: Vec::new(),
            states: Vec::new(),
        }
    }

    /// Parses RON text. The `format_version` is read first: a version this build does not read is
    /// [`StateTreeError::UnsupportedVersion`] even if the rest of the file has a newer shape. A
    /// version 1 file is migrated in memory (v2 is a superset, so only the version changes).
    pub fn from_ron(text: &str) -> Result<Self, StateTreeError> {
        let header: VersionHeader = ron::from_str(text).map_err(|e| StateTreeError::Parse(e.to_string()))?;
        if !(STATE_TREE_MIN_FORMAT_VERSION..=STATE_TREE_FORMAT_VERSION).contains(&header.format_version) {
            return Err(StateTreeError::UnsupportedVersion {
                found: header.format_version,
                supported: STATE_TREE_FORMAT_VERSION,
            });
        }
        let mut def: Self = ron::from_str(text).map_err(|e| StateTreeError::Parse(e.to_string()))?;
        def.format_version = STATE_TREE_FORMAT_VERSION;
        Ok(def)
    }

    /// Writes the definition as pretty RON.
    pub fn to_ron(&self) -> Result<String, StateTreeError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| StateTreeError::Write(e.to_string()))
    }
}

/// Rewrites the root `format_version: 1` of a `.statetree.ron` to 2 and keeps every other byte
/// (comments included): v2 only adds optional fields, so a v1 file means the same thing at v2
/// (`regia upgrade-project`, spec 38 § 4). A file already at v2 is returned unchanged; any other
/// version is [`StateTreeError::UnsupportedVersion`].
pub fn migrate_ron_v1_to_v2(text: &str) -> Result<String, StateTreeError> {
    let header: VersionHeader = ron::from_str(text).map_err(|e| StateTreeError::Parse(e.to_string()))?;
    match header.format_version {
        STATE_TREE_FORMAT_VERSION => return Ok(text.to_string()),
        1 => {}
        found => {
            return Err(StateTreeError::UnsupportedVersion {
                found,
                supported: STATE_TREE_FORMAT_VERSION,
            });
        }
    }
    let (start, end) = version_number_span(text)
        .ok_or_else(|| StateTreeError::Parse("no root `format_version` number".to_string()))?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&STATE_TREE_FORMAT_VERSION.to_string());
    out.push_str(&text[end..]);
    Ok(out)
}

/// The byte range of the number after the root struct's `format_version:` key, skipping strings
/// and comments and ignoring nested structs.
pub(crate) fn version_number_span(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let key = b"format_version";
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut nest = 1;
                i += 2;
                while i < bytes.len() && nest > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 1 && bytes[i..].starts_with(key) => {
                let mut j = i + key.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if bytes.get(j) != Some(&b':') {
                    i += 1;
                    continue;
                }
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                return (j > start).then_some((start, j));
            }
            _ => i += 1,
        }
    }
    None
}

/// Prefixes every state reference of `def` with `prefix/` (names, children, `State` targets, root).
fn prefixed(def: StateTreeDef, prefix: &str) -> StateTreeDef {
    let name = |n: &str| format!("{prefix}/{n}");
    let mut out = def;
    out.root = name(&out.root);
    for state in &mut out.states {
        state.name = name(&state.name);
        for child in &mut state.children {
            *child = name(child);
        }
        for t in &mut state.transitions {
            if let Target::State(target) = &mut t.target {
                *target = name(target);
            }
        }
    }
    out
}

impl StateTreeDef {
    /// Whether the definition declares any `includes`.
    pub fn has_includes(&self) -> bool {
        !self.includes.is_empty()
    }

    /// Returns the definition with every include inlined (state names prefixed `prefix/`) and the
    /// `includes` list emptied. `load` maps a project-relative path to RON text. The included
    /// tree's own `seed`, `state_key`, `ambient` and `description` are ignored; its `format_version`
    /// must be 1 or 2. Fails with `IncludeMissing`, `IncludeCycle` or `IncludeDepth`
    /// ([`MAX_INCLUDE_DEPTH`]).
    pub fn expand_includes(&self, load: &dyn Fn(&str) -> Option<String>) -> Result<Self, StateTreeError> {
        let mut stack = Vec::new();
        self.expand_inner(load, &mut stack)
    }

    fn expand_inner(
        &self,
        load: &dyn Fn(&str) -> Option<String>,
        stack: &mut Vec<String>,
    ) -> Result<Self, StateTreeError> {
        let mut out = self.clone();
        out.includes.clear();
        for include in &self.includes {
            if stack.contains(&include.path) {
                return Err(StateTreeError::IncludeCycle(include.path.clone()));
            }
            if stack.len() >= MAX_INCLUDE_DEPTH {
                return Err(StateTreeError::IncludeDepth);
            }
            let text =
                load(&include.path).ok_or_else(|| StateTreeError::IncludeMissing(include.path.clone()))?;
            let inner = Self::from_ron(&text)?;
            stack.push(include.path.clone());
            let expanded = inner.expand_inner(load, stack);
            stack.pop();
            out.states.extend(prefixed(expanded?, &include.prefix).states);
            if out.states.len() > MAX_STATES {
                return Err(StateTreeError::TooManyStates(out.states.len()));
            }
        }
        Ok(out)
    }
}

/// The `.statetree.ron` migration chain: the steps leave version 1 (to 2).
pub fn state_tree_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The serde shape fingerprint of `.statetree.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct reachable from [`StateTreeDef`], then the spellings of its enums.
pub fn state_tree_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<StateTreeDef>(),
        serde_field_names::<StateDef>(),
        serde_field_names::<TransitionDef>(),
        serde_field_names::<UtilityDef>(),
        serde_field_names::<IncludeDef>(),
        serde_variant_names::<Condition>(),
        serde_variant_names::<CompareOp>(),
        serde_variant_names::<Selection>(),
        serde_variant_names::<Trigger>(),
        serde_variant_names::<Target>(),
        serde_variant_names::<Literal>(),
        serde_variant_names::<Fallback>(),
    ])
}
