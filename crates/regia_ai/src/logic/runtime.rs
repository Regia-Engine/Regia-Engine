// SPDX-License-Identifier: MPL-2.0
//! The compiled logic graph and its tick (spec 102 § 2).
//!
//! One tick, in order: a paused graph returns at once; on the first tick after a start the `Start`
//! event is queued; due `Timer` arms are queued (arm order, then period); watched keys written since
//! the last tick queue one `BlackboardChanged` event each, in key order; then the inbox snapshot in
//! FIFO order (at most [`MAX_PENDING_EVENTS`], the oldest overflow is dropped and traced). Suspended
//! action chains resume first (one task tick each); then for each queued event the rules run in
//! (priority descending, declaration) order. Events emitted during the tick land in an inbox and are
//! delivered on the next tick, so a self-emitting rule pair advances one hop per tick.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_ecs::ecs::entity_disabling::Disabled;

use super::def::{
    Binding, LOGIC_FORMAT_VERSION, LogicEvent, LogicGraphDef, LogicGraphError, MAX_ACTIONS_PER_RULE,
    MAX_LOCALS, MAX_RULES, MAX_SET_ENTRIES, MAX_WORLD_BOARDS,
};
use super::{LogicAuthority, LogicInbox, LogicWorldBlackboards};
use crate::blackboard::{BbValue, Blackboard, assign_value};
use crate::state_tree::{Condition, Literal, MAX_CONDITION_DEPTH};
use crate::task::{BtContext, BtStatus, BtTask};
use crate::tree::BtTaskRegistry;

/// The most inbox events one tick takes; older ones beyond it are dropped with an `inbox_overflow`
/// trace line.
pub const MAX_PENDING_EVENTS: usize = 64;

/// The most trace lines a graph keeps; the oldest go first.
pub const MAX_TRACE_ENTRIES: usize = 64;

/// The most locals [`crate::logic::LogicGraphStatus`] shows.
pub const MAX_STATUS_LOCALS: usize = 64;

/// The built-in action tasks of phase 1, which need no registration.
pub const BUILTIN_TASKS: [&str; 4] = [
    "logic::SetBlackboard",
    "logic::EmitEvent",
    "logic::EnableEntity",
    "logic::DisableEntity",
];

/// What [`LogicGraph::reload`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicReloadOutcome {
    /// Running state carried over by rule name: locals, fire counts, cooldowns, timers, suspended
    /// chains and the tick count.
    Kept,
    /// A new `Start` rule must run, or a suspended chain's rule was removed, renamed or re-tasked:
    /// the graph starts again on its next tick (locals by name, fire counts and the inbox are kept).
    Restarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Local,
    Entity,
    /// A declared world blackboard, by its index in the graph's `world:` list (phase 3).
    World(usize),
}

/// The three scopes a graph reads, for one evaluation (spec 102 § 5).
struct Scopes<'a> {
    local: &'a Blackboard,
    entity: &'a Blackboard,
    boards: Option<&'a LogicWorldBlackboards>,
    /// The graph's declared world boards (`Scope::World` indexes it).
    names: &'a [String],
    empty: &'a Blackboard,
}

impl<'a> Scopes<'a> {
    fn of(
        local: &'a Blackboard,
        world: &'a World,
        entity: Entity,
        names: &'a [String],
        empty: &'a Blackboard,
    ) -> Self {
        Self {
            local,
            entity: world.get::<Blackboard>(entity).unwrap_or(empty),
            boards: world.get_resource::<LogicWorldBlackboards>(),
            names,
            empty,
        }
    }

    fn board(&self, scope: Scope) -> &'a Blackboard {
        match scope {
            Scope::Local => self.local,
            Scope::Entity => self.entity,
            Scope::World(i) => self
                .names
                .get(i)
                .and_then(|name| self.boards?.boards.get(name))
                .unwrap_or(self.empty),
        }
    }
}

/// The label a scoped key shows in the trace and in errors: `local/x`, `world/<board>/x` or the bare
/// entity key.
fn scoped_label(scope: Scope, key: &str, names: &[String]) -> String {
    match scope {
        Scope::Local => format!("local/{key}"),
        Scope::Entity => key.to_string(),
        Scope::World(i) => format!("world/{}/{key}", names.get(i).map_or("?", String::as_str)),
    }
}

enum KeyError {
    Empty,
    Unknown,
}

/// Splits a scoped key: `local/x`, `entity/x`, `world/<board>/x` (the board must be one of `world`,
/// the graph's declared boards) or a bare key (entity scope). Any other prefix, or an undeclared
/// board, is an unknown scope.
fn parse_key<'k>(key: &'k str, world: &[String]) -> Result<(Scope, &'k str), KeyError> {
    let (scope, rest) = if let Some(rest) = key.strip_prefix("local/") {
        (Scope::Local, rest)
    } else if let Some(rest) = key.strip_prefix("entity/") {
        (Scope::Entity, rest)
    } else if let Some(rest) = key.strip_prefix("world/") {
        let Some((board, rest)) = rest.split_once('/') else {
            return Err(KeyError::Unknown);
        };
        let Some(index) = world.iter().position(|b| b == board) else {
            return Err(KeyError::Unknown);
        };
        (Scope::World(index), rest)
    } else if key.contains('/') {
        return Err(KeyError::Unknown);
    } else {
        (Scope::Entity, key)
    };
    if rest.is_empty() {
        return Err(KeyError::Empty);
    }
    Ok((scope, rest))
}

fn key_error(err: KeyError, rule: &str, key: &str) -> LogicGraphError {
    match err {
        KeyError::Empty => LogicGraphError::BadValue {
            what: rule.to_string(),
            detail: format!("the key {key:?} is empty"),
        },
        KeyError::Unknown => LogicGraphError::UnknownScope {
            rule: rule.to_string(),
            key: key.to_string(),
        },
    }
}

/// A condition with every key resolved to its scope. Leaves keep the state-tree `Condition` (with the
/// prefix stripped) and evaluate through `Condition::eval` against the scope's blackboard.
enum CCond {
    Leaf(Scope, Condition),
    Not(Box<CCond>),
    All(Vec<CCond>),
    Any(Vec<CCond>),
}

impl CCond {
    fn compile(c: &Condition, rule: &str, world: &[String]) -> Result<Self, LogicGraphError> {
        let leaf = |key: &str, make: &dyn Fn(String) -> Condition| -> Result<Self, LogicGraphError> {
            let (scope, k) = parse_key(key, world).map_err(|e| key_error(e, rule, key))?;
            Ok(Self::Leaf(scope, make(k.to_string())))
        };
        Ok(match c {
            Condition::Always => Self::Leaf(Scope::Entity, Condition::Always),
            Condition::Key(k) => leaf(k, &Condition::Key)?,
            Condition::Has(k) => leaf(k, &Condition::Has)?,
            Condition::Compare { key, op, value } => leaf(key, &|k: String| Condition::Compare {
                key: k,
                op: *op,
                value: *value,
            })?,
            Condition::Text { key, value } => leaf(key, &|k: String| Condition::Text {
                key: k,
                value: value.clone(),
            })?,
            Condition::Not(inner) => Self::Not(Box::new(Self::compile(inner, rule, world)?)),
            Condition::All(items) => Self::All(
                items
                    .iter()
                    .map(|c| Self::compile(c, rule, world))
                    .collect::<Result<_, _>>()?,
            ),
            Condition::Any(items) => Self::Any(
                items
                    .iter()
                    .map(|c| Self::compile(c, rule, world))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }

    fn eval(&self, scopes: &Scopes<'_>) -> bool {
        match self {
            Self::Leaf(scope, c) => c.eval(scopes.board(*scope)),
            Self::Not(inner) => !inner.eval(scopes),
            Self::All(items) => items.iter().all(|c| c.eval(scopes)),
            Self::Any(items) => items.iter().any(|c| c.eval(scopes)),
        }
    }
}

enum CBinding {
    Literal(BbValue),
    /// A scoped key and its label (`local/x`, `world/<board>/x` or the bare entity key).
    Ref(Scope, String, String),
}

impl CBinding {
    fn compile(b: &Binding, rule: &str, world: &[String]) -> Result<Self, LogicGraphError> {
        match b {
            Binding::Literal(lit) => {
                if let Literal::Float(f) = lit
                    && !f.is_finite()
                {
                    return Err(LogicGraphError::BadValue {
                        what: rule.to_string(),
                        detail: "a literal float is not finite".to_string(),
                    });
                }
                Ok(Self::Literal(lit.to_value()))
            }
            Binding::BbRef(key) => {
                let (scope, k) = parse_key(key, world).map_err(|e| key_error(e, rule, key))?;
                Ok(Self::Ref(scope, k.to_string(), scoped_label(scope, k, world)))
            }
        }
    }

    fn resolve(&self, scopes: &Scopes<'_>) -> Option<BbValue> {
        match self {
            Self::Literal(v) => Some(v.clone()),
            Self::Ref(scope, k, _) => scopes.board(*scope).get(k).cloned(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Literal(v) => format!("{v:?}"),
            Self::Ref(_, _, label) => label.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Set,
    Increment,
    Append,
}

enum CAction {
    Set {
        key: CBinding,
        /// The key's scope and bare key, resolved at compile when the key is a literal: a steady
        /// write neither clones nor re-parses it.
        fixed: Option<(Scope, String)>,
        op: SetOp,
        value: Option<CBinding>,
    },
    Emit {
        event: CBinding,
        target: Option<CBinding>,
    },
    Toggle {
        enable: bool,
        target: Option<CBinding>,
    },
    Task {
        task: Box<dyn BtTask>,
        set: Vec<(String, CBinding)>,
        /// The resolved `set` values handed to the task, kept between runs so a steady run
        /// overwrites them in place.
        params: Vec<(String, BbValue)>,
    },
}

struct CActionSlot {
    path: String,
    action: CAction,
    /// A presentation action (spec 102 § 8): it runs on a peer without authority too.
    presentation: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum CEvent {
    Start,
    Timer(usize),
    Changed(usize),
    Named(String),
}

struct CRule {
    name: String,
    event: CEvent,
    priority: i32,
    cooldown: u64,
    cond: CCond,
    actions: Vec<CActionSlot>,
    fires: u64,
    matches: u64,
    last_fired: Option<u64>,
    cooldown_until: u64,
    /// The action a suspended (or running) chain is at.
    chain: Option<usize>,
    /// The chain's current action has started (its `reset` ran and `action_started` was traced).
    chain_started: bool,
}

struct CTimer {
    rule: usize,
    every: u64,
    jitter: u64,
    /// The period that fires next (1 for the first).
    period: u64,
    /// The graph tick that period is due on.
    due: u64,
}

struct CWatch {
    /// The key as the trace shows it (`local/x` or the bare entity key).
    label: String,
    scope: Scope,
    key: String,
    count: u64,
    value: Option<BbValue>,
}

/// One queued event of a tick.
enum QEvent {
    Start,
    Timer(usize),
    Changed(usize),
    Named(String),
}

/// The event a trace record names (spec 102 § 9 labels: `start`, `timer:<rule>`, `changed:<key>`
/// or the inbox name, which the record's text holds).
#[derive(Clone, Copy)]
enum TraceEvent {
    Start,
    Timer(usize),
    Changed(usize),
    Named,
}

impl TraceEvent {
    fn of(event: &QEvent) -> (Self, &str) {
        match event {
            QEvent::Start => (Self::Start, ""),
            QEvent::Timer(t) => (Self::Timer(*t), ""),
            QEvent::Changed(w) => (Self::Changed(*w), ""),
            QEvent::Named(name) => (Self::Named, name),
        }
    }
}

/// One trace line as a fixed-size record (ADR-7282): rules, actions, timers and watches by index
/// into this graph, the one variable-length part (an inbox event name, a failure detail) in the
/// slot's reused text. Formatted only when read.
#[derive(Clone, Copy)]
enum TraceKind {
    EventReceived {
        event: TraceEvent,
        dropped: bool,
    },
    /// Text: the event name.
    BreakpointEvent,
    RuleRejected {
        rule: usize,
        event: TraceEvent,
    },
    /// `cooldown`: ticks left (0: none, the rule fired).
    RuleMatched {
        rule: usize,
        event: TraceEvent,
        cooldown: u64,
    },
    ChainBusy {
        rule: usize,
    },
    BreakpointRule {
        rule: usize,
    },
    ActionStarted {
        rule: usize,
        action: usize,
    },
    /// Text: the failure detail.
    ActionFailed {
        rule: usize,
        action: usize,
    },
    /// Text: the dropped event name.
    InboxOverflow,
    ReloadKept,
    ReloadRestarted,
    /// A record of an earlier graph, formatted when a reload swapped this one in (its indices
    /// named the old rules). Text: the whole line.
    Frozen,
}

struct TraceSlot {
    tick: u64,
    kind: TraceKind,
    text: String,
}

/// The bounded trace: at most [`MAX_TRACE_ENTRIES`] records in a ring whose slots, text buffers
/// included, are reused, so a tick that traces allocates nothing once the ring is full.
#[derive(Default)]
struct TraceRing {
    slots: Vec<TraceSlot>,
    /// The oldest slot once the ring is full (0 before).
    head: usize,
    /// Records ever pushed (carried across a reload).
    total: u64,
}

impl TraceRing {
    /// Takes the next slot (the oldest when full) for a record and returns its text, cleared.
    fn push(&mut self, tick: u64, kind: TraceKind) -> &mut String {
        self.total = self.total.wrapping_add(1);
        let index = if self.slots.len() < MAX_TRACE_ENTRIES {
            self.slots.push(TraceSlot {
                tick,
                kind,
                text: String::with_capacity(SLOT_TEXT_CAPACITY),
            });
            self.slots.len() - 1
        } else {
            let index = self.head;
            self.head = (self.head + 1) % self.slots.len();
            let slot = &mut self.slots[index];
            slot.tick = tick;
            slot.kind = kind;
            slot.text.clear();
            index
        };
        &mut self.slots[index].text
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    /// The `i`th record, oldest first.
    fn get(&self, i: usize) -> Option<&TraceSlot> {
        let len = self.slots.len();
        (i < len).then(|| &self.slots[(self.head + i) % len])
    }
}

/// The starting capacity of a trace slot's text: an inbox name or a short detail fits, so a slot that
/// first holds a name after the ring filled does not allocate then.
const SLOT_TEXT_CAPACITY: usize = 32;

/// Source of [`LogicGraph`] trace identities (see `LogicGraph::trace_id`).
static NEXT_TRACE_ID: AtomicU64 = AtomicU64::new(1);

/// A compiled logic graph plus its running state (spec 102 § 2). Build with [`LogicGraph::compile`];
/// [`crate::AiPlugin`] ticks it every fixed step on the entity that holds it (one graph per entity).
#[derive(Component)]
pub struct LogicGraph {
    def: LogicGraphDef,
    rules: Vec<CRule>,
    /// Rule indices in evaluation order: priority descending, then declaration.
    order: Vec<usize>,
    timers: Vec<CTimer>,
    watches: Vec<CWatch>,
    locals: Blackboard,
    rng: u64,
    ticks: u64,
    started: bool,
    paused: bool,
    /// Events handed over by [`LogicGraph::send_event`] (or the inbox), delivered on the next tick.
    incoming: Vec<String>,
    /// Buffers of inbox event names this graph consumed, reused by its `logic::EmitEvent` actions
    /// (at most [`MAX_PENDING_EVENTS`]), so a graph that emits what it, or a peer that emits back,
    /// consumes allocates no event names (ADR-7282).
    spare_names: Vec<String>,
    queue: Vec<QEvent>,
    trace: TraceRing,
    /// Identifies this trace (kept across a reload, which moves the trace over), so a status that
    /// mirrors it can tell a continuation from another graph's trace.
    trace_id: u64,
    evaluations: u32,
    empty: Blackboard,
    /// Development breakpoints and disabled rules (spec 102 § 9, phase 4), set from
    /// `LogicGraphDebug` by the tick system; empty in shipping builds.
    debug: DebugLists,
    /// `step` was asked while paused: the next tick runs, then the graph pauses again.
    step_pending: bool,
    /// A breakpoint triggered during this tick: the graph pauses at its end.
    break_hit: bool,
}

/// The breakpoint and disabled-rule lists a graph applies (phase 4).
#[derive(Default, Clone, PartialEq)]
struct DebugLists {
    disabled: Vec<String>,
    pause_on_rule: Vec<String>,
    pause_on_event: Vec<String>,
}

fn bad(what: &str, detail: impl Into<String>) -> LogicGraphError {
    LogicGraphError::BadValue {
        what: what.to_string(),
        detail: detail.into(),
    }
}

/// The `entity_vars` and `world_vars` checks (v2, ADR-8900): counts, names, the board of a world
/// variable is declared, no duplicate, finite float defaults.
fn check_declared_vars(def: &LogicGraphDef) -> Result<(), LogicGraphError> {
    for (count, what) in [
        (def.entity_vars.len(), "entity variables"),
        (def.world_vars.len(), "world variables"),
    ] {
        if count > MAX_LOCALS {
            return Err(bad("", format!("{count} {what} (the limit is {MAX_LOCALS})")));
        }
    }
    let finite = |name: &str, value: &Literal| match value {
        Literal::Float(f) if !f.is_finite() => Err(bad(name, "the default is not finite")),
        _ => Ok(()),
    };
    let mut names = std::collections::BTreeSet::new();
    for var in &def.entity_vars {
        if var.name.is_empty() || var.name.contains('/') {
            return Err(bad(
                &var.name,
                "an entity variable name must be non-empty and contain no `/`",
            ));
        }
        if !names.insert(var.name.as_str()) {
            return Err(LogicGraphError::DuplicateName(var.name.clone()));
        }
        finite(&var.name, &var.default)?;
    }
    let mut keys = std::collections::BTreeSet::new();
    for var in &def.world_vars {
        let key = format!("world/{}/{}", var.board, var.name);
        if !def.world.contains(&var.board) {
            return Err(LogicGraphError::UnknownScope {
                rule: String::new(),
                key,
            });
        }
        if var.name.is_empty() || var.name.contains('/') {
            return Err(bad(
                &key,
                "a world variable name must be non-empty and contain no `/`",
            ));
        }
        finite(&key, &var.default)?;
        if !keys.insert(key.clone()) {
            return Err(LogicGraphError::DuplicateName(key));
        }
    }
    Ok(())
}

/// The declared default of `key` (v2): an entity variable for a bare or `entity/` key, a world
/// variable for `world/<board>/<name>`; `None` for locals and undeclared keys.
fn declared_default<'d>(def: &'d LogicGraphDef, key: &str) -> Option<&'d Literal> {
    if let Some(rest) = key.strip_prefix("world/") {
        let (board, name) = rest.split_once('/')?;
        return def
            .world_vars
            .iter()
            .find(|v| v.board == board && v.name == name)
            .map(|v| &v.default);
    }
    if key.starts_with("local/") {
        return None;
    }
    let name = key.strip_prefix("entity/").unwrap_or(key);
    def.entity_vars
        .iter()
        .find(|v| v.name == name)
        .map(|v| &v.default)
}

/// A built-in `logic::SetBlackboard` on a declared variable with a literal key must keep its type
/// (v2, ADR-8900): `set` of a literal of another kind, `increment` of a non-number or `append` to a
/// non-string variable is a compile error.
fn check_declared_write(
    def: &LogicGraphDef,
    path: &str,
    set: &BTreeMap<String, Binding>,
    rule: &str,
) -> Result<(), LogicGraphError> {
    if path != "logic::SetBlackboard" {
        return Ok(());
    }
    let Some(Binding::Literal(Literal::String(key))) = set.get("key") else {
        return Ok(());
    };
    let Some(declared) = declared_default(def, key) else {
        return Ok(());
    };
    let kind = |l: &Literal| match l {
        Literal::Bool(_) => "bool",
        Literal::Int(_) => "int",
        Literal::Float(_) => "float",
        Literal::String(_) => "string",
    };
    let op = match set.get("op") {
        Some(Binding::Literal(Literal::String(op))) => op.as_str(),
        _ => "set",
    };
    let fits = match (op, set.get("value")) {
        ("increment", _) => matches!(declared, Literal::Int(_) | Literal::Float(_)),
        ("append", _) => matches!(declared, Literal::String(_)),
        (_, Some(Binding::Literal(value))) => {
            std::mem::discriminant(value) == std::mem::discriminant(declared)
        }
        _ => true,
    };
    if fits {
        Ok(())
    } else {
        Err(bad(
            rule,
            format!(
                "logic::SetBlackboard {op} on {key}, declared {}, does not keep its type",
                kind(declared)
            ),
        ))
    }
}

fn locals_from(def: &LogicGraphDef) -> Blackboard {
    let mut bb = Blackboard::new();
    for local in &def.locals {
        bb.insert(local.name.clone(), local.default.to_value());
    }
    bb
}

fn literal_string(b: &Binding) -> Option<&str> {
    match b {
        Binding::Literal(Literal::String(s)) => Some(s),
        _ => None,
    }
}

fn compile_builtin(
    path: &str,
    set: &BTreeMap<String, Binding>,
    rule: &str,
    world: &[String],
) -> Result<CAction, LogicGraphError> {
    let allowed: &[&str] = match path {
        "logic::SetBlackboard" => &["key", "op", "value"],
        "logic::EmitEvent" => &["event", "target"],
        _ => &["target"],
    };
    if let Some(extra) = set.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(rule, format!("{path} has no parameter {extra:?}")));
    }
    let missing = |param: &str| LogicGraphError::MissingParam {
        rule: rule.to_string(),
        task: path.to_string(),
        param: param.to_string(),
    };
    let target = |set: &BTreeMap<String, Binding>| -> Result<Option<CBinding>, LogicGraphError> {
        match set.get("target") {
            None => Ok(None),
            Some(b @ Binding::BbRef(_)) => CBinding::compile(b, rule, world).map(Some),
            Some(Binding::Literal(_)) => Err(bad(
                rule,
                format!("{path}: target must be a BbRef to an entity value"),
            )),
        }
    };
    match path {
        "logic::SetBlackboard" => {
            let key = set.get("key").ok_or_else(|| missing("key"))?;
            let mut fixed = None;
            if let Binding::Literal(lit) = key {
                let Literal::String(k) = lit else {
                    return Err(bad(rule, "logic::SetBlackboard: key must be a string"));
                };
                let (scope, bare) = parse_key(k, world).map_err(|e| key_error(e, rule, k))?;
                fixed = Some((scope, bare.to_string()));
            }
            let op = match set.get("op") {
                None => SetOp::Set,
                Some(b) => match literal_string(b) {
                    Some("set") => SetOp::Set,
                    Some("increment") => SetOp::Increment,
                    Some("append") => SetOp::Append,
                    Some(other) => {
                        return Err(LogicGraphError::BadOp {
                            rule: rule.to_string(),
                            op: other.to_string(),
                        });
                    }
                    None => return Err(bad(rule, "logic::SetBlackboard: op must be a literal string")),
                },
            };
            let value = set.get("value");
            if value.is_none() && op != SetOp::Increment {
                return Err(missing("value"));
            }
            if let Some(Binding::Literal(lit)) = value {
                let ok = match op {
                    SetOp::Set => true,
                    SetOp::Increment => matches!(lit, Literal::Int(_) | Literal::Float(_)),
                    SetOp::Append => matches!(lit, Literal::String(_)),
                };
                if !ok {
                    return Err(bad(rule, "logic::SetBlackboard: value does not suit the op"));
                }
            }
            Ok(CAction::Set {
                key: CBinding::compile(key, rule, world)?,
                fixed,
                op,
                value: value.map(|v| CBinding::compile(v, rule, world)).transpose()?,
            })
        }
        "logic::EmitEvent" => {
            let event = set.get("event").ok_or_else(|| missing("event"))?;
            match event {
                Binding::Literal(Literal::String(s)) if s.is_empty() => {
                    return Err(bad(rule, "logic::EmitEvent: the event name is empty"));
                }
                Binding::Literal(Literal::String(_)) | Binding::BbRef(_) => {}
                Binding::Literal(_) => return Err(bad(rule, "logic::EmitEvent: event must be a string")),
            }
            Ok(CAction::Emit {
                event: CBinding::compile(event, rule, world)?,
                target: target(set)?,
            })
        }
        _ => Ok(CAction::Toggle {
            enable: path == "logic::EnableEntity",
            target: target(set)?,
        }),
    }
}

fn check_event(event: &LogicEvent, rule: &str, world: &[String]) -> Result<(), LogicGraphError> {
    let named = |what: &str, name: &str| {
        if name.is_empty() {
            Err(bad(rule, format!("the {what} name is empty")))
        } else {
            Ok(())
        }
    };
    match event {
        LogicEvent::Start => Ok(()),
        LogicEvent::Timer { every_ticks, .. } => {
            if *every_ticks == 0 {
                Err(bad(rule, "Timer every_ticks must be at least 1"))
            } else {
                Ok(())
            }
        }
        LogicEvent::BlackboardChanged { key } => parse_key(key, world)
            .map(|_| ())
            .map_err(|e| key_error(e, rule, key)),
        LogicEvent::EntityEvent(name) => {
            if name.is_empty() {
                Err(bad(rule, "the EntityEvent name is empty"))
            } else {
                Ok(())
            }
        }
        LogicEvent::OnSequenceMarker { sequence, marker } => {
            named("OnSequenceMarker sequence", sequence)?;
            named("OnSequenceMarker marker", marker)
        }
        LogicEvent::OnNetMessage { message } => named("OnNetMessage message", message),
        LogicEvent::InputAction { action } => named("InputAction action", action),
    }
}

impl LogicGraph {
    /// Compiles a definition, resolving task paths against `registry` (the built-in `logic::*`
    /// tasks need no registration). Fails with the first problem found, in this order: version,
    /// the `world:` list (board names), rule and local counts, local names and values, then each
    /// rule (name, event arm, condition depth and keys, action count, each action's task and `set`).
    pub fn compile(def: &LogicGraphDef, registry: &BtTaskRegistry) -> Result<Self, LogicGraphError> {
        // A v1 definition built in code is a valid v2 one (v2 only adds the declaration lists).
        if def.format_version != LOGIC_FORMAT_VERSION && def.format_version != 1 {
            return Err(LogicGraphError::UnsupportedVersion {
                found: def.format_version,
                supported: LOGIC_FORMAT_VERSION,
            });
        }
        if def.world.len() > MAX_WORLD_BOARDS {
            return Err(bad(
                "",
                format!(
                    "{} world blackboards (the limit is {MAX_WORLD_BOARDS})",
                    def.world.len()
                ),
            ));
        }
        let mut boards = std::collections::BTreeSet::new();
        for board in &def.world {
            if board.is_empty() || board.contains('/') {
                return Err(bad(
                    board,
                    "a world blackboard name must be non-empty and contain no `/`",
                ));
            }
            if !boards.insert(board.as_str()) {
                return Err(LogicGraphError::DuplicateName(board.clone()));
            }
        }
        let world = def.world.as_slice();
        if def.rules.len() > MAX_RULES {
            return Err(LogicGraphError::TooManyRules(def.rules.len()));
        }
        if def.locals.len() > MAX_LOCALS {
            return Err(bad(
                "",
                format!("{} locals (the limit is {MAX_LOCALS})", def.locals.len()),
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for local in &def.locals {
            if local.name.is_empty() || local.name.contains('/') {
                return Err(bad(
                    &local.name,
                    "a local name must be non-empty and contain no `/`",
                ));
            }
            if !names.insert(local.name.as_str()) {
                return Err(LogicGraphError::DuplicateName(local.name.clone()));
            }
            if let Literal::Float(f) = local.default
                && !f.is_finite()
            {
                return Err(bad(&local.name, "the default is not finite"));
            }
        }
        check_declared_vars(def)?;
        let mut rule_names = std::collections::BTreeSet::new();
        let mut rules = Vec::with_capacity(def.rules.len());
        let mut timers = Vec::new();
        let mut watch_keys: BTreeMap<String, (Scope, String)> = BTreeMap::new();
        for (index, r) in def.rules.iter().enumerate() {
            if r.name.is_empty() {
                return Err(bad("", format!("rule {index} has an empty name")));
            }
            if !rule_names.insert(r.name.as_str()) {
                return Err(LogicGraphError::DuplicateName(r.name.clone()));
            }
            check_event(&r.event, &r.name, world)?;
            if r.condition.depth() > MAX_CONDITION_DEPTH {
                return Err(LogicGraphError::ConditionTooDeep(r.name.clone()));
            }
            if !r.condition.is_finite() {
                return Err(bad(&r.name, "a Compare constant is not finite"));
            }
            let cond = CCond::compile(&r.condition, &r.name, world)?;
            if r.actions.len() > MAX_ACTIONS_PER_RULE {
                return Err(LogicGraphError::TooManyActions(r.name.clone()));
            }
            let mut actions = Vec::with_capacity(r.actions.len());
            for a in &r.actions {
                if a.set.len() > MAX_SET_ENTRIES {
                    return Err(bad(
                        &r.name,
                        format!(
                            "{} has {} set entries (the limit is {MAX_SET_ENTRIES})",
                            a.task,
                            a.set.len()
                        ),
                    ));
                }
                let action = if BUILTIN_TASKS.contains(&a.task.as_str()) {
                    check_declared_write(def, &a.task, &a.set, &r.name)?;
                    compile_builtin(&a.task, &a.set, &r.name, world)?
                } else {
                    let task = registry
                        .construct(&a.task)
                        .ok_or_else(|| LogicGraphError::UnknownTask {
                            rule: r.name.clone(),
                            task: a.task.clone(),
                        })?;
                    let mut set = Vec::with_capacity(a.set.len());
                    for (k, b) in &a.set {
                        if k.is_empty() {
                            return Err(bad(&r.name, "a set key is empty"));
                        }
                        set.push((k.clone(), CBinding::compile(b, &r.name, world)?));
                    }
                    CAction::Task {
                        task,
                        set,
                        params: Vec::new(),
                    }
                };
                actions.push(CActionSlot {
                    path: a.task.clone(),
                    action,
                    presentation: registry.is_presentation(&a.task),
                });
            }
            let event = match &r.event {
                LogicEvent::Start => CEvent::Start,
                LogicEvent::Timer {
                    every_ticks,
                    jitter_ticks,
                } => {
                    timers.push(CTimer {
                        rule: index,
                        every: u64::from(*every_ticks),
                        jitter: u64::from(*jitter_ticks),
                        period: 1,
                        due: 0,
                    });
                    CEvent::Timer(timers.len() - 1)
                }
                LogicEvent::BlackboardChanged { key } => {
                    // Checked by `check_event`.
                    let (scope, k) = parse_key(key, world).map_err(|e| key_error(e, &r.name, key))?;
                    let label = scoped_label(scope, k, world);
                    watch_keys.insert(label.clone(), (scope, k.to_string()));
                    // The watch index is fixed once every key is known (below).
                    CEvent::Named(label)
                }
                LogicEvent::EntityEvent(name) => CEvent::Named(name.clone()),
                // The phase-3 arms arrive through the inbox under their own spellings
                // (`LogicEvent::inbox_name`), raised by the sequencer, net and input bridges.
                other => match other.inbox_name() {
                    Some(name) => CEvent::Named(name),
                    None => return Err(bad(&r.name, "unsupported event")),
                },
            };
            rules.push(CRule {
                name: r.name.clone(),
                event,
                priority: r.priority,
                cooldown: u64::from(r.cooldown_ticks),
                cond,
                actions,
                fires: 0,
                matches: 0,
                last_fired: None,
                cooldown_until: 0,
                chain: None,
                chain_started: false,
            });
        }
        // Watches in key order; rules on a `BlackboardChanged` arm point at their watch.
        let watches: Vec<CWatch> = watch_keys
            .into_iter()
            .map(|(label, (scope, key))| CWatch {
                label,
                scope,
                key,
                count: 0,
                value: None,
            })
            .collect();
        for (rule, def_rule) in rules.iter_mut().zip(&def.rules) {
            if let (LogicEvent::BlackboardChanged { .. }, CEvent::Named(label)) =
                (&def_rule.event, &rule.event)
                && let Some(w) = watches.iter().position(|w| &w.label == label)
            {
                rule.event = CEvent::Changed(w);
            }
        }
        let mut order: Vec<usize> = (0..rules.len()).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(rules[i].priority), i));
        Ok(Self {
            locals: locals_from(def),
            def: def.clone(),
            rules,
            order,
            timers,
            watches,
            rng: def.seed,
            ticks: 0,
            started: false,
            paused: false,
            incoming: Vec::new(),
            spare_names: Vec::new(),
            queue: Vec::new(),
            trace: TraceRing::default(),
            trace_id: NEXT_TRACE_ID.fetch_add(1, Ordering::Relaxed),
            evaluations: 0,
            empty: Blackboard::new(),
            debug: DebugLists::default(),
            step_pending: false,
            break_hit: false,
        })
    }

    /// The definition this graph was compiled from.
    pub fn def(&self) -> &LogicGraphDef {
        &self.def
    }

    /// Rule names in evaluation order (priority descending, then declaration).
    pub fn rule_order(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(|&i| self.rules[i].name.as_str())
    }

    /// Graph ticks since the last start: the start tick is 1, and each later tick adds the fixed
    /// steps that passed (more than one when the AI tick budget deferred the graph).
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Whether the graph has run its start tick.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Whether the graph is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Pauses the graph: no timer, watch, inbox event or action chain advances until
    /// [`LogicGraph::resume`]; inbox events wait.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Lets a paused graph tick again.
    pub fn resume(&mut self) {
        self.paused = false;
        self.step_pending = false;
    }

    /// Runs exactly one more tick and pauses again (spec 102 § 13 P4.1): a paused graph runs its
    /// next tick (inbox events included) and pauses at its end; a running graph pauses at the end of
    /// its next tick.
    pub fn step(&mut self) {
        self.paused = true;
        self.step_pending = true;
    }

    /// Whether the next tick runs although the graph is paused (a [`LogicGraph::step`] is pending).
    pub fn is_stepping(&self) -> bool {
        self.paused && self.step_pending
    }

    /// Sets the development debug lists (spec 102 § 9): rules named in `disabled` never match;
    /// a fire of a rule in `pause_on_rule`, or an inbox event named in `pause_on_event`, pauses the
    /// graph at the end of that tick. Copies only when a list changed, so a steady debug component
    /// allocates nothing.
    pub fn set_debug(&mut self, disabled: &[String], pause_on_rule: &[String], pause_on_event: &[String]) {
        if self.debug.disabled != disabled {
            self.debug.disabled = disabled.to_vec();
        }
        if self.debug.pause_on_rule != pause_on_rule {
            self.debug.pause_on_rule = pause_on_rule.to_vec();
        }
        if self.debug.pause_on_event != pause_on_event {
            self.debug.pause_on_event = pause_on_event.to_vec();
        }
    }

    /// The debug lists in force: `(disabled, pause_on_rule, pause_on_event)`.
    pub fn debug_lists(&self) -> (&[String], &[String], &[String]) {
        (
            &self.debug.disabled,
            &self.debug.pause_on_rule,
            &self.debug.pause_on_event,
        )
    }

    /// Whether the graph has a rule named `name`.
    pub fn has_rule(&self, name: &str) -> bool {
        self.rules.iter().any(|r| r.name == name)
    }

    /// The graph locals (`local/` scope).
    pub fn locals(&self) -> &Blackboard {
        &self.locals
    }

    /// The trace, oldest first (at most [`MAX_TRACE_ENTRIES`] lines), formatted now: the graph
    /// keeps typed records and makes text only when it is read. A reader that must not allocate
    /// formats into its own buffers with [`LogicGraph::write_trace_line`].
    pub fn trace(&self) -> impl Iterator<Item = String> + '_ {
        (0..self.trace.len()).map(|i| {
            let mut line = String::new();
            self.write_trace_line(i, &mut line);
            line
        })
    }

    /// Trace lines held now (at most [`MAX_TRACE_ENTRIES`]).
    pub fn trace_len(&self) -> usize {
        self.trace.len()
    }

    /// Trace records ever written by this graph (and the graphs a reload replaced): the line
    /// [`LogicGraph::trace_len`]` - 1` is record `trace_total() - 1`.
    pub fn trace_total(&self) -> u64 {
        self.trace.total
    }

    /// An identity of this trace, unique in the process and kept across [`LogicGraph::reload`]
    /// (which moves the trace into the new graph): a mirror of the trace that saw the same
    /// identity can update by [`LogicGraph::trace_total`] alone.
    pub fn trace_id(&self) -> u64 {
        self.trace_id
    }

    /// Appends trace line `i` (oldest first) to `out`, exactly as [`LogicGraph::trace`] prints it;
    /// nothing for an index past the end. Allocates only when `out` must grow.
    pub fn write_trace_line(&self, i: usize, out: &mut String) {
        if let Some(slot) = self.trace.get(i) {
            self.write_record(slot, out);
        }
    }

    fn rule_name(&self, rule: usize) -> &str {
        self.rules.get(rule).map_or("?", |r| r.name.as_str())
    }

    fn write_event(&self, event: TraceEvent, text: &str, out: &mut String) {
        match event {
            TraceEvent::Start => out.push_str("start"),
            TraceEvent::Timer(t) => {
                out.push_str("timer:");
                out.push_str(self.timers.get(t).map_or("?", |t| self.rule_name(t.rule)));
            }
            TraceEvent::Changed(w) => {
                out.push_str("changed:");
                out.push_str(self.watches.get(w).map_or("?", |w| w.label.as_str()));
            }
            TraceEvent::Named => out.push_str(text),
        }
    }

    /// The text of one record: `tick <n>: <kind> <args>`, the spellings of spec 102 § 9.
    fn write_record(&self, slot: &TraceSlot, out: &mut String) {
        if let TraceKind::Frozen = slot.kind {
            out.push_str(&slot.text);
            return;
        }
        let _ = write!(out, "tick {}: ", slot.tick);
        let action_path = |rule: usize, action: usize| {
            self.rules
                .get(rule)
                .and_then(|r| r.actions.get(action))
                .map_or("?", |a| a.path.as_str())
        };
        match slot.kind {
            TraceKind::EventReceived { event, dropped } => {
                out.push_str("event_received ");
                self.write_event(event, &slot.text, out);
                if dropped {
                    out.push_str(" (dropped: no rule)");
                }
            }
            TraceKind::BreakpointEvent => {
                out.push_str("breakpoint event ");
                out.push_str(&slot.text);
            }
            TraceKind::RuleRejected { rule, event } => {
                let _ = write!(out, "rule_rejected {} on ", self.rule_name(rule));
                self.write_event(event, &slot.text, out);
            }
            TraceKind::RuleMatched {
                rule,
                event,
                cooldown,
            } => {
                let _ = write!(out, "rule_matched {} on ", self.rule_name(rule));
                self.write_event(event, &slot.text, out);
                if cooldown > 0 {
                    let _ = write!(out, " (cooldown {cooldown})");
                }
            }
            TraceKind::ChainBusy { rule } => {
                let _ = write!(out, "chain_busy {}", self.rule_name(rule));
            }
            TraceKind::BreakpointRule { rule } => {
                let _ = write!(out, "breakpoint rule {}", self.rule_name(rule));
            }
            TraceKind::ActionStarted { rule, action } => {
                let _ = write!(
                    out,
                    "action_started {} {action} {}",
                    self.rule_name(rule),
                    action_path(rule, action)
                );
            }
            TraceKind::ActionFailed { rule, action } => {
                let _ = write!(
                    out,
                    "action_failed {} {action} {}: {}",
                    self.rule_name(rule),
                    action_path(rule, action),
                    slot.text
                );
            }
            TraceKind::InboxOverflow => {
                out.push_str("inbox_overflow ");
                out.push_str(&slot.text);
            }
            TraceKind::ReloadKept => out.push_str("reload_kept"),
            TraceKind::ReloadRestarted => out.push_str("reload_restarted"),
            TraceKind::Frozen => {}
        }
    }

    /// `(name, fires, matches, last fired tick, cooldown remaining, chain busy)` per rule, in
    /// declaration order.
    pub fn rule_stats(&self) -> impl Iterator<Item = (&str, u64, u64, Option<u64>, u64, bool)> {
        self.rules.iter().map(|r| {
            (
                r.name.as_str(),
                r.fires,
                r.matches,
                r.last_fired,
                r.cooldown_until.saturating_sub(self.ticks),
                r.chain.is_some(),
            )
        })
    }

    /// Rule evaluations (an event arm that matched, condition checked) in the last tick. Bounded by
    /// `MAX_PENDING_EVENTS` (+ timers, watches and start) times [`MAX_RULES`].
    pub fn evaluations_last_tick(&self) -> u32 {
        self.evaluations
    }

    /// Queues an event for the next tick (the `LogicInbox` drain and `logic.emit` use this).
    pub fn send_event(&mut self, name: impl Into<String>) {
        self.incoming.push(name.into());
    }

    /// Fixed steps until the next timer is due, when the graph has one and has started; the AI
    /// tick budget wakes a deferred graph on that step.
    pub fn steps_until_timer(&self) -> Option<u64> {
        if !self.started || self.paused {
            return None;
        }
        self.timers.iter().map(|t| t.due.saturating_sub(self.ticks)).min()
    }

    /// Records a trace line of `kind` at the current tick, with `text` as its variable part.
    fn record(&mut self, kind: TraceKind, text: &str) {
        self.trace.push(self.ticks, kind).push_str(text);
    }

    /// Turns every record into its text (a reload is about to hand the trace to a graph whose
    /// indices name other rules). Allocates: reloads are rare.
    fn freeze_trace(&mut self) {
        let lines: Vec<String> = (0..self.trace.len())
            .map(|i| {
                let mut line = String::new();
                self.write_trace_line(i, &mut line);
                line
            })
            .collect();
        for (i, line) in lines.into_iter().enumerate() {
            let len = self.trace.slots.len();
            let index = (self.trace.head + i) % len;
            let slot = &mut self.trace.slots[index];
            slot.kind = TraceKind::Frozen;
            slot.text = line;
        }
    }

    /// SplitMix64 (Steele, Lea, Flood 2014), one stream per graph instance.
    fn draw(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Schedules timer `i`'s current period: `period * every + draw % (jitter + 1)`.
    fn arm_timer(&mut self, i: usize) {
        let d = self.draw();
        let t = &mut self.timers[i];
        t.due = t
            .period
            .saturating_mul(t.every)
            .saturating_add(d % (t.jitter + 1));
    }

    /// The graph's scopes on `entity` in `world` (the world boards from [`LogicWorldBlackboards`]).
    fn scopes<'a>(&'a self, world: &'a World, entity: Entity) -> Scopes<'a> {
        Scopes::of(&self.locals, world, entity, &self.def.world, &self.empty)
    }

    fn watch_state(watch: &CWatch, scopes: &Scopes<'_>) -> (u64, bool) {
        let bb = scopes.board(watch.scope);
        (
            bb.write_count(&watch.key),
            bb.get(&watch.key) == watch.value.as_ref(),
        )
    }

    fn baseline_watches(&mut self, world: &World, entity: Entity) {
        let baselines: Vec<(u64, Option<BbValue>)> = {
            let scopes = self.scopes(world, entity);
            self.watches
                .iter()
                .map(|w| {
                    let bb = scopes.board(w.scope);
                    (bb.write_count(&w.key), bb.get(&w.key).cloned())
                })
                .collect()
        };
        for (w, (count, value)) in self.watches.iter_mut().zip(baselines) {
            w.count = count;
            w.value = value;
        }
    }

    /// Makes sure every declared world board exists, so the status and the agent list it before
    /// its first write.
    fn declare_boards(&self, world: &mut World) {
        if self.def.world.is_empty() {
            return;
        }
        let missing = world
            .get_resource::<LogicWorldBlackboards>()
            .is_none_or(|b| self.def.world.iter().any(|n| !b.boards.contains_key(n)));
        if missing {
            let mut boards = world.get_resource_or_insert_with(LogicWorldBlackboards::default);
            for name in &self.def.world {
                if !boards.boards.contains_key(name) {
                    boards.boards.insert(name.clone(), Blackboard::new());
                }
            }
        }
    }

    /// Writes the default of every declared entity and world variable (v2, ADR-8900) the entity's
    /// `Blackboard` or the world board does not hold yet; a key already there keeps its value. Runs
    /// when the graph starts, before the watches take their baseline, so a seed raises no
    /// `BlackboardChanged`.
    fn seed_declared_vars(&self, world: &mut World, entity: Entity) {
        if !self.def.entity_vars.is_empty()
            && let Ok(mut e) = world.get_entity_mut(entity)
        {
            let missing = e
                .get::<Blackboard>()
                .is_none_or(|bb| self.def.entity_vars.iter().any(|v| !bb.contains(&v.name)));
            if missing {
                if e.get::<Blackboard>().is_none() {
                    e.insert(Blackboard::new());
                }
                if let Some(mut bb) = e.get_mut::<Blackboard>() {
                    for var in &self.def.entity_vars {
                        if !bb.contains(&var.name) {
                            bb.insert(var.name.clone(), var.default.to_value());
                        }
                    }
                }
            }
        }
        if !self.def.world_vars.is_empty() {
            let mut boards = world.get_resource_or_insert_with(LogicWorldBlackboards::default);
            for var in &self.def.world_vars {
                let board = boards.boards.entry(var.board.clone()).or_default();
                if !board.contains(&var.name) {
                    board.insert(var.name.clone(), var.default.to_value());
                }
            }
        }
    }

    /// Returns the graph to not-started: suspended chains are reset and dropped, cooldowns, timers,
    /// the tick count and the locals start over, pending events are discarded. The next tick runs
    /// the `Start` rules. Fire counts stay.
    pub fn restart(&mut self, ctx: &mut BtContext<'_>) {
        self.abort_chains(ctx);
        for r in &mut self.rules {
            r.cooldown_until = 0;
        }
        for t in &mut self.timers {
            t.period = 1;
            t.due = 0;
        }
        self.locals = locals_from(&self.def);
        self.rng = self.def.seed;
        self.ticks = 0;
        self.started = false;
        self.incoming.clear();
    }

    fn abort_chains(&mut self, ctx: &mut BtContext<'_>) {
        for r in &mut self.rules {
            if let Some(ai) = r.chain.take()
                && r.chain_started
                && let Some(CActionSlot {
                    action: CAction::Task { task, .. },
                    ..
                }) = r.actions.get_mut(ai)
            {
                task.reset(ctx);
            }
            r.chain_started = false;
        }
    }

    /// Hot reload (spec 102 § 7): swaps in `next`, an already-compiled graph. Running state carries
    /// over by rule name ([`LogicReloadOutcome::Kept`]) unless a `Start` rule that did not exist
    /// before must run, or a suspended chain's rule was removed or now runs other tasks
    /// ([`LogicReloadOutcome::Restarted`]). Either way locals carry by name (new locals take their
    /// defaults, removed ones vanish), fire counts carry by rule name, and the inbox, trace and
    /// pause flag are kept.
    pub fn reload(&mut self, mut next: LogicGraph, ctx: &mut BtContext<'_>) -> LogicReloadOutcome {
        let mut restart = false;
        if self.started {
            let old_start: Vec<&str> = self
                .rules
                .iter()
                .filter(|r| r.event == CEvent::Start)
                .map(|r| r.name.as_str())
                .collect();
            restart |= next
                .rules
                .iter()
                .any(|r| r.event == CEvent::Start && !old_start.contains(&r.name.as_str()));
            for r in &self.rules {
                if r.chain.is_none() {
                    continue;
                }
                let same = next.rules.iter().find(|n| n.name == r.name).is_some_and(|n| {
                    n.actions.len() == r.actions.len()
                        && n.actions.iter().zip(&r.actions).all(|(a, b)| a.path == b.path)
                });
                restart |= !same;
            }
        }
        // Locals by name: declared in `next` keep their value; removed declarations vanish.
        let old_declared: Vec<&str> = self.def.locals.iter().map(|l| l.name.as_str()).collect();
        for (key, value) in &self.locals.values {
            let declared_next = next.def.locals.iter().any(|l| &l.name == key);
            if declared_next || !old_declared.contains(&key.as_str()) {
                next.locals.insert(key.clone(), value.clone());
            }
        }
        // Fire counts by name.
        for n in &mut next.rules {
            if let Some(o) = self.rules.iter().find(|o| o.name == n.name) {
                n.fires = o.fires;
                n.matches = o.matches;
                n.last_fired = o.last_fired;
            }
        }
        next.incoming = std::mem::take(&mut self.incoming);
        self.freeze_trace();
        next.trace = std::mem::take(&mut self.trace);
        next.trace_id = self.trace_id;
        next.paused = self.paused;
        next.step_pending = self.step_pending;
        next.debug = std::mem::take(&mut self.debug);
        let outcome = if restart || !self.started {
            self.abort_chains(ctx);
            if restart {
                LogicReloadOutcome::Restarted
            } else {
                LogicReloadOutcome::Kept
            }
        } else {
            next.started = true;
            next.ticks = self.ticks;
            next.rng = self.rng;
            for ni in 0..next.rules.len() {
                let Some(oi) = self.rules.iter().position(|o| o.name == next.rules[ni].name) else {
                    continue;
                };
                next.rules[ni].cooldown_until = self.rules[oi].cooldown_until;
                if let Some(ai) = self.rules[oi].chain.take() {
                    // Same task paths (checked above): the running tasks move over with their state.
                    for (n, o) in next.rules[ni].actions.iter_mut().zip(&mut self.rules[oi].actions) {
                        if let (CAction::Task { task: nt, .. }, CAction::Task { task: ot, .. }) =
                            (&mut n.action, &mut o.action)
                        {
                            std::mem::swap(nt, ot);
                        }
                    }
                    next.rules[ni].chain = Some(ai);
                    next.rules[ni].chain_started = self.rules[oi].chain_started;
                }
            }
            // Timers of rules that kept their name and period carry their schedule; new ones arm
            // from the current tick.
            for ti in 0..next.timers.len() {
                let name = &next.rules[next.timers[ti].rule].name;
                let (every, jitter) = (next.timers[ti].every, next.timers[ti].jitter);
                let old = self
                    .timers
                    .iter()
                    .find(|o| &self.rules[o.rule].name == name && o.every == every && o.jitter == jitter);
                match old {
                    Some(o) => {
                        next.timers[ti].period = o.period;
                        next.timers[ti].due = o.due;
                    }
                    None => {
                        next.timers[ti].period = next.ticks / every + 1;
                        next.arm_timer(ti);
                    }
                }
            }
            // Watches of the same key keep their baseline; new ones start from the current values.
            next.declare_boards(ctx.world);
            next.baseline_watches(ctx.world, ctx.entity);
            for w in &mut next.watches {
                if let Some(o) = self.watches.iter().find(|o| o.label == w.label) {
                    w.count = o.count;
                    w.value = o.value.clone();
                }
            }
            LogicReloadOutcome::Kept
        };
        *self = next;
        match outcome {
            LogicReloadOutcome::Kept => self.record(TraceKind::ReloadKept, ""),
            LogicReloadOutcome::Restarted => self.record(TraceKind::ReloadRestarted, ""),
        }
        outcome
    }

    /// Ticks the graph once. `steps` is the fixed steps since its previous tick (1 without an AI tick
    /// budget); timers count those steps. A paused graph returns at once, unless a
    /// [`LogicGraph::step`] is pending: then this tick runs and the graph pauses at its end. A
    /// breakpoint (`set_debug`) that triggers during the tick pauses the graph at its end too.
    pub fn tick(&mut self, ctx: &mut BtContext<'_>, steps: u64) {
        if self.paused && !self.step_pending {
            return;
        }
        self.evaluations = 0;
        // The names of last tick's inbox events become spare buffers for this graph's emits.
        for event in self.queue.drain(..) {
            if let QEvent::Named(name) = event
                && self.spare_names.len() < MAX_PENDING_EVENTS
            {
                self.spare_names.push(name);
            }
        }
        if self.started {
            self.ticks = self.ticks.saturating_add(steps.max(1));
        } else {
            self.started = true;
            self.ticks = 1;
            for i in 0..self.timers.len() {
                self.timers[i].period = 1;
                self.arm_timer(i);
            }
            self.declare_boards(ctx.world);
            self.seed_declared_vars(ctx.world, ctx.entity);
            self.baseline_watches(ctx.world, ctx.entity);
            self.queue.push(QEvent::Start);
        }
        // Timers, arm order then period.
        for i in 0..self.timers.len() {
            let mut fired = 0;
            while self.timers[i].due <= self.ticks && fired < MAX_PENDING_EVENTS {
                self.queue.push(QEvent::Timer(i));
                self.timers[i].period += 1;
                self.arm_timer(i);
                fired += 1;
            }
        }
        // Watched keys written since the last tick, in key order.
        {
            let scopes = Scopes::of(&self.locals, ctx.world, ctx.entity, &self.def.world, &self.empty);
            for (i, w) in self.watches.iter_mut().enumerate() {
                let (count, same) = Self::watch_state(w, &scopes);
                if count == w.count && same {
                    continue;
                }
                w.count = count;
                // The remembered value is overwritten in place (a string reuses its buffer).
                match (&mut w.value, scopes.board(w.scope).get(&w.key)) {
                    (Some(old), Some(new)) => assign_value(old, new),
                    (slot, new) => *slot = new.cloned(),
                }
                self.queue.push(QEvent::Changed(i));
            }
        }
        // The inbox snapshot, FIFO; the oldest overflow is dropped.
        let mut incoming = std::mem::take(&mut self.incoming);
        // Traced at the end of the tick, so the lines of the events kept cannot push them out of the
        // bounded trace.
        let overflow: Vec<String> = if incoming.len() > MAX_PENDING_EVENTS {
            let excess = incoming.len() - MAX_PENDING_EVENTS;
            incoming.drain(..excess).collect()
        } else {
            Vec::new()
        };
        self.queue.extend(incoming.drain(..).map(QEvent::Named));
        self.incoming = incoming;

        let authority = ctx.world.get_resource::<LogicAuthority>().map(|a| a.0);
        let authoritative = authority.is_none_or(|check| check(ctx.entity, ctx.world));

        // Suspended chains resume first, one task tick each. (`run_chain` never reads the order.)
        let order = std::mem::take(&mut self.order);
        for &ri in &order {
            if self.rules[ri].chain.is_some() {
                self.run_chain(ri, ctx, authoritative);
            }
        }
        self.order = order;

        let queue = std::mem::take(&mut self.queue);
        for event in &queue {
            self.deliver(event, ctx, authoritative);
        }
        self.queue = queue;
        for dropped in overflow {
            self.record(TraceKind::InboxOverflow, &dropped);
        }
        // Breakpoints and a pending step pause the graph at the end of the tick (spec 102 § 9).
        if std::mem::take(&mut self.step_pending) {
            self.paused = true;
        }
        if std::mem::take(&mut self.break_hit) {
            self.paused = true;
        }
    }

    fn event_matches(rule: &CRule, event: &QEvent) -> bool {
        match (event, &rule.event) {
            (QEvent::Start, CEvent::Start) => true,
            (QEvent::Timer(t), CEvent::Timer(rt)) => t == rt,
            (QEvent::Changed(w), CEvent::Changed(rw)) => w == rw,
            (QEvent::Named(name), CEvent::Named(rn)) => name == rn,
            _ => false,
        }
    }

    fn deliver(&mut self, event: &QEvent, ctx: &mut BtContext<'_>, authoritative: bool) {
        // The label is typed: a timer or watch by index, an inbox name copied into the record's
        // reused text (ADR-7282), so delivering allocates nothing.
        let (label, name) = TraceEvent::of(event);
        let armed = self
            .order
            .iter()
            .any(|&ri| Self::event_matches(&self.rules[ri], event));
        if let QEvent::Named(name) = event
            && self.debug.pause_on_event.contains(name)
        {
            self.break_hit = true;
            self.record(TraceKind::BreakpointEvent, name);
        }
        self.record(
            TraceKind::EventReceived {
                event: label,
                dropped: !armed,
            },
            name,
        );
        if !armed {
            return;
        }
        // Neither the rules nor `run_chain` read the order, so it is lent out for the loop.
        let order = std::mem::take(&mut self.order);
        for &ri in &order {
            if !Self::event_matches(&self.rules[ri], event) {
                continue;
            }
            // A disabled rule (development debug) never matches.
            if !self.debug.disabled.is_empty() && self.debug.disabled.contains(&self.rules[ri].name) {
                continue;
            }
            self.evaluations = self.evaluations.saturating_add(1);
            let holds = self.rules[ri].cond.eval(&self.scopes(ctx.world, ctx.entity));
            if !holds {
                self.record(
                    TraceKind::RuleRejected {
                        rule: ri,
                        event: label,
                    },
                    name,
                );
                continue;
            }
            self.rules[ri].matches += 1;
            let remaining = self.rules[ri].cooldown_until.saturating_sub(self.ticks);
            self.record(
                TraceKind::RuleMatched {
                    rule: ri,
                    event: label,
                    cooldown: remaining,
                },
                name,
            );
            if remaining > 0 {
                continue;
            }
            if self.rules[ri].chain.is_some() {
                self.record(TraceKind::ChainBusy { rule: ri }, "");
                continue;
            }
            if !self.debug.pause_on_rule.is_empty() && self.debug.pause_on_rule.contains(&self.rules[ri].name)
            {
                self.break_hit = true;
                self.record(TraceKind::BreakpointRule { rule: ri }, "");
            }
            let rule = &mut self.rules[ri];
            rule.fires += 1;
            rule.last_fired = Some(self.ticks);
            rule.cooldown_until = self.ticks.saturating_add(rule.cooldown);
            if rule.actions.is_empty() {
                continue;
            }
            rule.chain = Some(0);
            rule.chain_started = false;
            self.run_chain(ri, ctx, authoritative);
        }
        self.order = order;
    }

    /// Runs rule `ri`'s chain from its current action until an action is `Running` (suspended), one
    /// fails, or the chain ends. Each task ticks at most once per call.
    fn run_chain(&mut self, ri: usize, ctx: &mut BtContext<'_>, authoritative: bool) {
        loop {
            let Some(ai) = self.rules[ri].chain else {
                return;
            };
            if ai >= self.rules[ri].actions.len() {
                self.rules[ri].chain = None;
                return;
            }
            if !self.rules[ri].chain_started {
                self.rules[ri].chain_started = true;
                self.record(TraceKind::ActionStarted { rule: ri, action: ai }, "");
                if let CAction::Task { task, .. } = &mut self.rules[ri].actions[ai].action {
                    task.reset(ctx);
                }
            }
            let result = if authoritative || self.rules[ri].actions[ai].presentation {
                let Self {
                    rules,
                    locals,
                    empty,
                    def,
                    spare_names,
                    ..
                } = self;
                execute(
                    &mut rules[ri].actions[ai].action,
                    locals,
                    empty,
                    &def.world,
                    spare_names,
                    ctx,
                )
            } else {
                // A peer without authority skips sim actions (spec 102 § 8): every built-in action
                // and every registered task not marked presentation (`BtTaskRegistry::mark_presentation`).
                Ok(BtStatus::Success)
            };
            match result {
                Ok(BtStatus::Success) => {
                    self.rules[ri].chain = Some(ai + 1);
                    self.rules[ri].chain_started = false;
                }
                Ok(BtStatus::Running) => return,
                Ok(BtStatus::Failure) | Err(_) => {
                    let detail = match &result {
                        Err(d) => d.as_str(),
                        _ => "failure",
                    };
                    self.record(TraceKind::ActionFailed { rule: ri, action: ai }, detail);
                    self.rules[ri].chain = None;
                    self.rules[ri].chain_started = false;
                    return;
                }
            }
        }
    }
}

/// The live entity `value` names; `binding` labels the failure (only built on that path).
fn entity_of(value: Option<BbValue>, world: &World, binding: &CBinding) -> Result<Entity, String> {
    match value {
        Some(BbValue::Entity(bits)) => Entity::try_from_bits(bits)
            .filter(|e| world.get_entity(*e).is_ok())
            .ok_or_else(|| format!("target {} is not a live entity", binding.label())),
        Some(_) => Err(format!("target {} is not an entity value", binding.label())),
        None => Err(format!("target {} is not set", binding.label())),
    }
}

/// Runs one action step. `Err` carries the failure detail for the trace.
fn execute(
    action: &mut CAction,
    locals: &mut Blackboard,
    empty: &Blackboard,
    names: &[String],
    spare_names: &mut Vec<String>,
    ctx: &mut BtContext<'_>,
) -> Result<BtStatus, String> {
    let entity = ctx.entity;
    match action {
        CAction::Set {
            key,
            fixed,
            op,
            value,
        } => {
            // A literal key was resolved at compile; only a `BbRef` key is read (and copied) here.
            let dynamic_path;
            let (path, scope, k): (&str, Scope, &str) = match (fixed.as_ref(), &*key) {
                (Some((scope, k)), CBinding::Literal(BbValue::String(path))) => (path, *scope, k),
                _ => {
                    let resolved = key.resolve(&Scopes::of(locals, ctx.world, entity, names, empty));
                    let Some(BbValue::String(p)) = resolved else {
                        return Err(format!("key {} is not a string", key.label()));
                    };
                    dynamic_path = p;
                    let (scope, k) = parse_key(&dynamic_path, names)
                        .map_err(|_| format!("key {dynamic_path:?} has an unknown scope"))?;
                    (dynamic_path.as_str(), scope, k)
                }
            };
            // A literal value is borrowed; a `BbRef` is read now (a copy: the write below needs the
            // world mutably).
            let referenced;
            let value: Option<&BbValue> = match &*value {
                None => None,
                Some(CBinding::Literal(v)) => Some(v),
                Some(b) => {
                    let s = Scopes::of(locals, ctx.world, entity, names, empty);
                    referenced = b.resolve(&s).ok_or_else(|| format!("{} is not set", b.label()))?;
                    Some(&referenced)
                }
            };
            if scope == Scope::Entity && ctx.world.get::<Blackboard>(entity).is_none() {
                let Ok(mut e) = ctx.world.get_entity_mut(entity) else {
                    return Err("the graph entity is gone".to_string());
                };
                e.insert(Blackboard::new());
            }
            let mut entity_bb;
            let mut boards;
            let bb: &mut Blackboard = match scope {
                Scope::Local => locals,
                Scope::Entity => {
                    entity_bb = ctx
                        .world
                        .get_mut::<Blackboard>(entity)
                        .ok_or_else(|| "the graph entity has no blackboard".to_string())?;
                    &mut entity_bb
                }
                Scope::World(i) => {
                    let name = names
                        .get(i)
                        .ok_or_else(|| format!("key {path:?} names no declared world board"))?;
                    boards = ctx
                        .world
                        .get_resource_or_insert_with(LogicWorldBlackboards::default);
                    // Looked up by reference first: `entry` would copy the name on every write.
                    if !boards.boards.contains_key(name) {
                        boards.boards.insert(name.clone(), Blackboard::new());
                    }
                    boards
                        .boards
                        .get_mut(name)
                        .ok_or_else(|| format!("key {path:?} names no declared world board"))?
                }
            };
            match op {
                SetOp::Set => bb.assign(k, value.ok_or_else(|| "no value".to_string())?),
                SetOp::Increment => {
                    let delta = value.unwrap_or(&BbValue::Int(1));
                    let new = match (bb.get(k), delta) {
                        (None, d @ (BbValue::Int(_) | BbValue::Float(_))) => d.clone(),
                        (Some(BbValue::Int(a)), BbValue::Int(b)) => BbValue::Int(a.saturating_add(*b)),
                        (Some(BbValue::Int(a)), BbValue::Float(b)) => BbValue::Float(*a as f32 + b),
                        (Some(BbValue::Float(a)), BbValue::Int(b)) => BbValue::Float(a + *b as f32),
                        (Some(BbValue::Float(a)), BbValue::Float(b)) => BbValue::Float(a + b),
                        _ => return Err(format!("{path} cannot be incremented")),
                    };
                    bb.assign(k, &new);
                }
                SetOp::Append => match (bb.get(k), value) {
                    (None, Some(s @ BbValue::String(_))) => bb.assign(k, s),
                    (Some(BbValue::String(_)), Some(BbValue::String(s))) => {
                        bb.append_str(k, s);
                    }
                    _ => return Err(format!("{path} cannot be appended to")),
                },
            }
            Ok(BtStatus::Success)
        }
        CAction::Emit { event, target } => {
            let (referenced, target_value) = {
                let s = Scopes::of(locals, ctx.world, entity, names, empty);
                (
                    match &*event {
                        CBinding::Literal(_) => None,
                        CBinding::Ref(..) => event.resolve(&s),
                    },
                    target.as_ref().map(|t| (t.resolve(&s), t)),
                )
            };
            let text = match (&*event, &referenced) {
                (CBinding::Literal(BbValue::String(name)), _) | (_, Some(BbValue::String(name))) => {
                    name.as_str()
                }
                _ => "",
            };
            if text.is_empty() {
                return Err(format!("event {} is not a non-empty string", event.label()));
            }
            let target = match target_value {
                None => entity,
                Some((v, binding)) => entity_of(v, ctx.world, binding)?,
            };
            // A spare buffer from an event this graph consumed: emitting allocates no name.
            let mut name = spare_names.pop().unwrap_or_default();
            name.clear();
            name.push_str(text);
            let Ok(mut e) = ctx.world.get_entity_mut(target) else {
                return Err("the target entity is gone".to_string());
            };
            match e.get_mut::<LogicInbox>() {
                Some(mut inbox) => inbox.events.push(name),
                None => {
                    e.insert(LogicInbox { events: vec![name] });
                }
            }
            Ok(BtStatus::Success)
        }
        CAction::Toggle { enable, target } => {
            let target = match target {
                None => entity,
                Some(t) => {
                    let v = {
                        let s = Scopes::of(locals, ctx.world, entity, names, empty);
                        t.resolve(&s)
                    };
                    entity_of(v, ctx.world, t)?
                }
            };
            let Ok(mut e) = ctx.world.get_entity_mut(target) else {
                return Err("the target entity is gone".to_string());
            };
            if *enable {
                e.remove::<Disabled>();
            } else {
                e.insert(Disabled);
            }
            Ok(BtStatus::Success)
        }
        CAction::Task { task, set, params } => {
            if !set.is_empty() {
                {
                    let s = Scopes::of(locals, ctx.world, entity, names, empty);
                    if params.len() != set.len() {
                        params.clear();
                        params.extend(set.iter().map(|(k, _)| (k.clone(), BbValue::Bool(false))));
                    }
                    // Overwritten in place: a steady run of a task with parameters allocates nothing.
                    for ((_, b), (_, param)) in set.iter().zip(params.iter_mut()) {
                        match b {
                            CBinding::Literal(v) => assign_value(param, v),
                            CBinding::Ref(scope, k, _) => {
                                let v = s
                                    .board(*scope)
                                    .get(k)
                                    .ok_or_else(|| format!("{} is not set", b.label()))?;
                                assign_value(param, v);
                            }
                        }
                    }
                }
                task.logic_params(params);
                if ctx.world.get::<Blackboard>(entity).is_none()
                    && let Ok(mut e) = ctx.world.get_entity_mut(entity)
                {
                    e.insert(Blackboard::new());
                }
                if let Some(mut bb) = ctx.world.get_mut::<Blackboard>(entity) {
                    for (k, v) in params.iter() {
                        bb.assign(k, v);
                    }
                }
            }
            Ok(task.tick(ctx))
        }
    }
}

/// The scopes a logic graph reads (§ 4.2, § 5), lent out for debugger reads (ADR-8021: the editor's
/// watch list, hover values and evaluate box): one condition or one key resolved by the graph's own
/// scope rule outside a tick. Reads only: nothing ticks, fires, traces or writes. Build it from a
/// running graph with [`LogicGraph::scope_view`], or from copies of its blackboards (the editor's
/// paused snapshot).
#[derive(Clone, Copy)]
pub struct LogicScopeView<'a> {
    /// The `local/` scope (the graph's locals).
    pub locals: &'a Blackboard,
    /// The entity scope (bare keys and `entity/`): the graph entity's `Blackboard`.
    pub entity: &'a Blackboard,
    /// The world boards (`world/<board>/`), when any exist.
    pub boards: Option<&'a LogicWorldBlackboards>,
    /// The boards the graph declares (its `world:` list); `world/` reads only these.
    pub world: &'a [String],
}

impl<'a> LogicScopeView<'a> {
    fn scopes(&self) -> Scopes<'a> {
        static EMPTY: std::sync::LazyLock<Blackboard> = std::sync::LazyLock::new(Blackboard::new);
        Scopes {
            local: self.locals,
            entity: self.entity,
            boards: self.boards,
            names: self.world,
            empty: &EMPTY,
        }
    }

    /// Evaluates `condition` as a rule guard of the graph would.
    ///
    /// # Errors
    ///
    /// The condition does not compile in this graph: too deep, a constant that is not finite, an
    /// empty key or a scope the graph does not declare (`unknown_scope`).
    pub fn evaluate(&self, condition: &Condition) -> Result<bool, LogicGraphError> {
        const WHAT: &str = "evaluate";
        if condition.depth() > MAX_CONDITION_DEPTH {
            return Err(LogicGraphError::ConditionTooDeep(WHAT.to_string()));
        }
        if !condition.is_finite() {
            return Err(bad(WHAT, "a Compare constant is not finite"));
        }
        let compiled = CCond::compile(condition, WHAT, self.world)?;
        Ok(compiled.eval(&self.scopes()))
    }

    /// The value of `key` (`local/x`, `world/<board>/x`, `entity/x` or a bare entity key); `None`
    /// when the scope holds no such key.
    ///
    /// # Errors
    ///
    /// An empty key or a scope the graph does not declare (`unknown_scope`).
    pub fn read(&self, key: &str) -> Result<Option<&'a BbValue>, LogicGraphError> {
        let (scope, bare) = parse_key(key, self.world).map_err(|e| key_error(e, "watch", key))?;
        Ok(self.scopes().board(scope).get(bare))
    }
}

impl LogicGraph {
    /// This graph's scopes on `entity` in `world` (see [`LogicScopeView`]).
    pub fn scope_view<'a>(&'a self, world: &'a World, entity: Entity) -> LogicScopeView<'a> {
        static EMPTY: std::sync::LazyLock<Blackboard> = std::sync::LazyLock::new(Blackboard::new);
        LogicScopeView {
            locals: &self.locals,
            entity: world.get::<Blackboard>(entity).unwrap_or(&EMPTY),
            boards: world.get_resource::<LogicWorldBlackboards>(),
            world: &self.def.world,
        }
    }
}
