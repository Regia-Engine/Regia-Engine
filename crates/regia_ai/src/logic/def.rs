// SPDX-License-Identifier: MPL-2.0
//! The authored logic graph (`.logic.ron`, format version 2, `docs/specs/102-logic-graph.md` § 3):
//! a flat list of event → condition → action rules, graph locals, typed entity and world variable
//! declarations and the timer seed.
//!
//! The v1 schema was complete for every phase of spec 102: the `OnSequenceMarker`, `OnNetMessage`
//! and `InputAction` event arms and the `world:` list were in it from phase 1 (they failed compile
//! with `unsupported_feature` until phase 3 gave them their meaning). Version 2 (ADR-8900, P-177)
//! adds the optional `entity_vars` and `world_vars` lists: a type (the default's literal kind) and a
//! default for an entity key or a world board key. A v1 file is a valid v2 body, so the migration is
//! the number only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::state_tree::{Condition, Literal};

/// Current `.logic.ron` format version (2 since ADR-8900: typed entity and world variables).
pub const LOGIC_FORMAT_VERSION: u32 = 2;

/// The most rules one graph may declare.
pub const MAX_RULES: usize = 256;

/// The most actions one rule may chain.
pub const MAX_ACTIONS_PER_RULE: usize = 16;

/// The most locals one graph may declare.
pub const MAX_LOCALS: usize = 64;

/// The most entries one action's `set` map may hold.
pub const MAX_SET_ENTRIES: usize = 32;

/// The most world blackboards one graph may declare (ADR-7268).
pub const MAX_WORLD_BOARDS: usize = 16;

/// The inbox spelling of an `OnSequenceMarker { sequence, marker }` event: the sequencer bridge
/// pushes `sequence_marker:<sequence>#<marker>` into the bound entity's `LogicInbox` when playback
/// crosses the marker (spec 102 § 4.1, ADR-7268).
pub fn sequence_marker_event(sequence: &str, marker: &str) -> String {
    format!("sequence_marker:{sequence}#{marker}")
}

/// The inbox spelling of an `OnNetMessage { message }` event: `net_message:<message>`, pushed when
/// the net layer applies a message that targets the entity.
pub fn net_message_event(message: &str) -> String {
    format!("net_message:{message}")
}

/// The inbox spelling of an `InputAction { action }` event: `input_action:<action>`, pushed on a
/// locally controlled entity when the bound action fires.
pub fn input_action_event(action: &str) -> String {
    format!("input_action:{action}")
}

/// What makes a rule a candidate (the left column of the graph).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicEvent {
    /// The graph instance starts (its first tick after spawn or restart).
    Start,
    /// Every `every_ticks` fixed steps (first fire after one period), plus a seeded jitter of
    /// `0..=jitter_ticks` steps per period.
    Timer {
        /// The period in fixed steps (at least 1).
        every_ticks: u32,
        /// The largest extra delay of one period, drawn from the graph seed (default 0).
        #[serde(default)]
        jitter_ticks: u32,
    },
    /// The key (`local/x`, `entity/x` or bare `x`) was written at least once since the graph's last
    /// tick; coalesced to one event per key per tick.
    BlackboardChanged {
        /// The watched key.
        key: String,
    },
    /// A named event arrived in the graph's `LogicInbox`.
    EntityEvent(String),
    /// A playing sequence bound to this entity reached a marker (inbox event
    /// [`sequence_marker_event`]).
    OnSequenceMarker {
        /// Sequence asset path.
        sequence: String,
        /// Marker name.
        marker: String,
    },
    /// A net message targeting this entity was applied (inbox event [`net_message_event`]).
    OnNetMessage {
        /// Message name.
        message: String,
    },
    /// A bound input action fired on a locally controlled entity (inbox event
    /// [`input_action_event`]).
    InputAction {
        /// Input action name.
        action: String,
    },
}

impl LogicEvent {
    /// The `LogicInbox` event name this arm matches: the name of an `EntityEvent`, the bridge
    /// spellings of the sequence, net and input arms; `None` for `Start`, `Timer` and
    /// `BlackboardChanged`, which the graph raises itself.
    pub fn inbox_name(&self) -> Option<String> {
        match self {
            Self::Start | Self::Timer { .. } | Self::BlackboardChanged { .. } => None,
            Self::EntityEvent(name) => Some(name.clone()),
            Self::OnSequenceMarker { sequence, marker } => Some(sequence_marker_event(sequence, marker)),
            Self::OnNetMessage { message } => Some(net_message_event(message)),
            Self::InputAction { action } => Some(input_action_event(action)),
        }
    }
}

/// A value an action writes or passes: a literal, or a reference to a blackboard key in one of the
/// graph's scopes (`local/x`, `entity/x` or bare `x`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Binding {
    /// A constant.
    Literal(Literal),
    /// The current value of a scoped blackboard key, read when the action runs.
    BbRef(String),
}

/// One action of a rule: a task path and the bindings it runs with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionDef {
    /// A built-in `logic::*` task or a task path registered in the `BtTaskRegistry`.
    pub task: String,
    /// Parameters. Registered tasks see them written into the entity blackboard before each tick;
    /// built-in tasks read them directly.
    #[serde(default)]
    pub set: BTreeMap<String, Binding>,
}

/// One rule: when `event` happens and `condition` holds, run `actions` in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDef {
    /// Unique, non-empty name (the rule's identity across hot reloads).
    pub name: String,
    /// The event arm.
    pub event: LogicEvent,
    /// Higher runs first among rules on the same event; ties go to declaration order.
    #[serde(default)]
    pub priority: i32,
    /// Fixed steps after a fire during which the rule matches but does not run its actions.
    #[serde(default)]
    pub cooldown_ticks: u32,
    /// The guard (default `Always`); keys name their scope by prefix.
    #[serde(default)]
    pub condition: Condition,
    /// The action chain, in execution order.
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

/// A graph-local variable and its initial value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalDef {
    /// Unique, non-empty name (read as `local/<name>`).
    pub name: String,
    /// The value the local starts with.
    pub default: Literal,
}

/// A declared entity variable (v2, ADR-8900): the entity `Blackboard` key `name` (read as `name` or
/// `entity/name`) with a type (the default's kind) and a default. When the graph starts, the key is
/// set to the default unless the entity's blackboard already holds it; a built-in
/// `logic::SetBlackboard` `set` of a literal of another type is a compile error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityVarDef {
    /// Unique, non-empty name without `/`.
    pub name: String,
    /// The key's type and its value when the entity lacks it.
    pub default: Literal,
}

/// A declared world variable (v2, ADR-8900): the key `name` of the declared world board `board`
/// (read as `world/<board>/<name>`) with a type and a default, written into the board when the graph
/// starts unless the board already holds the key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldVarDef {
    /// One of the graph's `world:` boards.
    pub board: String,
    /// Non-empty key without `/`, unique within its board.
    pub name: String,
    /// The key's type and its value when the board lacks it.
    pub default: Literal,
}

/// The `.logic.ron` document (spec 102 § 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicGraphDef {
    /// Format version, [`LOGIC_FORMAT_VERSION`].
    pub format_version: u32,
    /// What the graph is for.
    #[serde(default)]
    pub description: String,
    /// Seed of the timer-jitter generator (SplitMix64).
    #[serde(default)]
    pub seed: u64,
    /// Graph locals.
    #[serde(default)]
    pub locals: Vec<LocalDef>,
    /// Declared world blackboards (`world/<name>/key`, the `LogicWorldBlackboards` resource); a graph
    /// may only touch the boards it declares.
    #[serde(default)]
    pub world: Vec<String>,
    /// Typed entity variables (v2; at most [`MAX_LOCALS`]; not written when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entity_vars: Vec<EntityVarDef>,
    /// Typed world variables of the declared boards (v2; at most [`MAX_LOCALS`]; not written when
    /// empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub world_vars: Vec<WorldVarDef>,
    /// The rules, in declaration order.
    #[serde(default)]
    pub rules: Vec<RuleDef>,
}

/// Why a logic graph could not be read or compiled. [`LogicGraphError::reason`] is the snake_case
/// code the agent protocol reports.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LogicGraphError {
    /// The RON text did not parse.
    #[error("logic graph RON is invalid: {0}")]
    Parse(String),
    /// The RON text could not be written.
    #[error("logic graph could not be written as RON: {0}")]
    Write(String),
    /// The `format_version` is not one this build reads.
    #[error("logic graph format_version {found} is not supported (this build reads version {supported})")]
    UnsupportedVersion {
        /// Version found.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },
    /// More than [`MAX_RULES`] rules.
    #[error("logic graph has {0} rules (the limit is {MAX_RULES})")]
    TooManyRules(usize),
    /// A rule chains more than [`MAX_ACTIONS_PER_RULE`] actions.
    #[error("rule {0:?} has more than {MAX_ACTIONS_PER_RULE} actions")]
    TooManyActions(String),
    /// A condition nests deeper than `MAX_CONDITION_DEPTH`.
    #[error("rule {0:?} has a condition nested deeper than 16")]
    ConditionTooDeep(String),
    /// Two rules or two locals share a name.
    #[error("logic graph declares the name {0:?} twice")]
    DuplicateName(String),
    /// A task path is neither a built-in `logic::*` task nor registered.
    #[error("rule {rule:?} references unregistered task {task:?}")]
    UnknownTask {
        /// The rule.
        rule: String,
        /// The task path.
        task: String,
    },
    /// A key names a scope that does not exist (or an undeclared world blackboard).
    #[error(
        "rule {rule:?} uses the key {key:?}, whose scope is unknown (use local/, entity/, world/<declared board>/ or a bare key)"
    )]
    UnknownScope {
        /// The rule (empty for a graph-level key).
        rule: String,
        /// The key.
        key: String,
    },
    /// A value is out of range: an empty name or key, a non-finite number, a zero timer period,
    /// more than [`MAX_LOCALS`] locals, [`MAX_WORLD_BOARDS`] world boards or [`MAX_SET_ENTRIES`]
    /// `set` entries, a world board name that is empty or holds `/`, or a built-in parameter of the
    /// wrong type.
    #[error("{what}: {detail}")]
    BadValue {
        /// The rule or local (empty for the graph).
        what: String,
        /// What is wrong.
        detail: String,
    },
    /// A built-in task's `op` is not one it knows.
    #[error("rule {rule:?}: unknown op {op:?}")]
    BadOp {
        /// The rule.
        rule: String,
        /// The op.
        op: String,
    },
    /// A built-in task lacks a required parameter.
    #[error("rule {rule:?}: task {task} needs the parameter {param:?}")]
    MissingParam {
        /// The rule.
        rule: String,
        /// The task.
        task: String,
        /// The parameter.
        param: String,
    },
    /// The graph uses a feature this build does not have (none since phase 3; kept for the agent
    /// protocol's stable reason list).
    #[error("{what}: {feature} is not supported yet")]
    UnsupportedFeature {
        /// The rule (empty for the graph).
        what: String,
        /// The feature.
        feature: String,
    },
}

impl LogicGraphError {
    /// The snake_case reason used on the agent protocol (`data.reason`, spec 102 § 10).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Parse(_) | Self::Write(_) => "parse",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::TooManyRules(_) => "too_many_rules",
            Self::TooManyActions(_) => "too_many_actions",
            Self::ConditionTooDeep(_) => "condition_too_deep",
            Self::DuplicateName(_) => "duplicate_name",
            Self::UnknownTask { .. } => "unknown_task",
            Self::UnknownScope { .. } => "unknown_scope",
            Self::BadValue { .. } => "bad_value",
            Self::BadOp { .. } => "bad_op",
            Self::MissingParam { .. } => "missing_param",
            Self::UnsupportedFeature { .. } => "unsupported_feature",
        }
    }

    /// The rule (or local) the error is about, when it names one.
    pub fn rule(&self) -> Option<&str> {
        let name = match self {
            Self::TooManyActions(r) | Self::ConditionTooDeep(r) | Self::DuplicateName(r) => r,
            Self::UnknownTask { rule, .. }
            | Self::UnknownScope { rule, .. }
            | Self::BadOp { rule, .. }
            | Self::MissingParam { rule, .. } => rule,
            Self::BadValue { what, .. } | Self::UnsupportedFeature { what, .. } => what,
            _ => return None,
        };
        (!name.is_empty()).then_some(name.as_str())
    }
}

/// The version field alone, read before the body. Named like the document so a file that spells the
/// struct name (`LogicGraphDef(...)`, as spec 102 § 3 does) reads as well as one that does not.
#[derive(Deserialize)]
#[serde(rename = "LogicGraphDef")]
struct VersionHeader {
    format_version: u32,
}

impl LogicGraphDef {
    /// An empty graph at the current version.
    pub fn new() -> Self {
        Self {
            format_version: LOGIC_FORMAT_VERSION,
            description: String::new(),
            seed: 0,
            locals: Vec::new(),
            world: Vec::new(),
            entity_vars: Vec::new(),
            world_vars: Vec::new(),
            rules: Vec::new(),
        }
    }

    /// Parses RON text. The `format_version` is read first, so a file from a newer build (or a
    /// version 0) is [`LogicGraphError::UnsupportedVersion`] even when the rest of it has a newer
    /// shape. A v1 file is migrated ([`logic_migration_sources`]): its body is a valid v2 body, so
    /// only the number moves to [`LOGIC_FORMAT_VERSION`].
    pub fn from_ron(text: &str) -> Result<Self, LogicGraphError> {
        let header: VersionHeader = ron::from_str(text).map_err(|e| LogicGraphError::Parse(e.to_string()))?;
        if header.format_version != LOGIC_FORMAT_VERSION
            && !logic_migration_sources().contains(&header.format_version)
        {
            return Err(LogicGraphError::UnsupportedVersion {
                found: header.format_version,
                supported: LOGIC_FORMAT_VERSION,
            });
        }
        let mut def: Self = ron::from_str(text).map_err(|e| LogicGraphError::Parse(e.to_string()))?;
        def.format_version = LOGIC_FORMAT_VERSION;
        Ok(def)
    }

    /// Writes the graph as pretty RON.
    pub fn to_ron(&self) -> Result<String, LogicGraphError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| LogicGraphError::Write(e.to_string()))
    }
}

impl Default for LogicGraphDef {
    fn default() -> Self {
        Self::new()
    }
}

/// The versions [`LogicGraphDef::from_ron`] migrates from: 1 (v2 only adds the optional
/// `entity_vars` and `world_vars` lists, so the step is the number only; ADR-8900).
pub fn logic_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The serde shape fingerprint of `.logic.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct reachable from [`LogicGraphDef`], then the spellings of its enums.
pub fn logic_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<LogicGraphDef>(),
        serde_field_names::<LocalDef>(),
        serde_field_names::<EntityVarDef>(),
        serde_field_names::<WorldVarDef>(),
        serde_field_names::<RuleDef>(),
        serde_field_names::<ActionDef>(),
        serde_variant_names::<LogicEvent>(),
        serde_variant_names::<Binding>(),
        serde_variant_names::<Condition>(),
        serde_variant_names::<crate::state_tree::CompareOp>(),
        serde_variant_names::<Literal>(),
    ])
}
