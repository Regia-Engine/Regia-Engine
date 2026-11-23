// SPDX-License-Identifier: MPL-2.0
//! The compiled state tree and its tick.
//!
//! One tick runs in this order (spec 38 § 6): paused trees return at once; ambient tasks tick;
//! queued events (each may take one transition); the tasks of every active state (root to leaf);
//! held-transition counters advance; then the tick and completion transitions. A transition
//! exits the active states below the common prefix (leaf first, resetting their tasks), then
//! enters the target's ancestors that were not active and the target itself, then descends by
//! each state's [`Selection`] to a leaf. States entered this way are first ticked on the next
//! tick. Each tick takes at most one transition from the tick/completion pool, so a cycle of
//! always-true transitions advances one step per tick instead of looping.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use regia_app::prelude::*;

use super::def::{Condition, Fallback, Literal, MAX_CONDITION_DEPTH, MAX_STATES};
use super::def::{STATE_TREE_FORMAT_VERSION, STATE_TREE_MIN_FORMAT_VERSION};
use super::def::{Selection, StateTreeDef, StateTreeError, Target, Trigger, UtilityDef};
use crate::blackboard::{BbValue, Blackboard};
use crate::task::{BtContext, BtStatus, BtTask};
use crate::tree::BtTaskRegistry;

/// Resolves a task path, including a `bt:` bridge (ADR-4543).
fn construct_task(
    path: &str,
    state: &str,
    registry: &BtTaskRegistry,
    load: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn BtTask>, StateTreeError> {
    if let Some(task) = crate::bt_bridge::compile_bt_task(path, state, registry, load)? {
        return Ok(task);
    }
    registry
        .construct(path)
        .ok_or_else(|| StateTreeError::UnknownTask {
            state: state.to_string(),
            task: path.to_string(),
        })
}

/// The most events a tree holds between ticks; further events are dropped.
pub const MAX_PENDING_EVENTS: usize = 64;

/// The most trace entries a tree keeps; the oldest are dropped first.
pub const MAX_TRACE_ENTRIES: usize = 64;

/// Where a state tree is in its life.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StateTreeRunStatus {
    /// Not ticked yet: no state is active.
    #[default]
    NotStarted,
    /// At least one state is active.
    Running,
    /// A `Succeed` transition fired, or the root's tasks (or its leaf's) all succeeded with no
    /// transition taking over.
    Succeeded,
    /// A `Fail` transition fired, a task failed with no transition handling it, or the root could
    /// not be entered.
    Failed,
}

/// One thing the tree did, kept in a bounded trace for debugging.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceKind {
    /// A state became active.
    Entered(String),
    /// A state stopped being active.
    Exited(String),
    /// A transition was taken.
    Transition {
        /// State that owns the transition.
        from: String,
        /// What it went to (`"<succeed>"` / `"<fail>"` for tree-finishing targets).
        to: String,
        /// Trigger label (`tick`, `event:<name>`, `succeeded`, `failed`, `completed`).
        trigger: String,
        /// The transition's priority.
        priority: i32,
    },
    /// A matching transition could not be taken because the target (or what it would select)
    /// could not be entered.
    Rejected {
        /// State that owns the transition.
        from: String,
        /// The target state.
        to: String,
    },
    /// An event matched no transition.
    EventIgnored(String),
    /// An event was dropped because [`MAX_PENDING_EVENTS`] were already queued.
    EventDropped(String),
    /// The root could not be entered when the tree started.
    StartFailed,
    /// The tree finished.
    Finished(StateTreeRunStatus),
}

/// A trace entry: the tick it happened on and what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    /// Tick number (the first tick is 1).
    pub tick: u64,
    /// What happened.
    pub kind: TraceKind,
}

impl fmt::Display for TraceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tick {}: ", self.tick)?;
        match &self.kind {
            TraceKind::Entered(s) => write!(f, "enter {s}"),
            TraceKind::Exited(s) => write!(f, "exit {s}"),
            TraceKind::Transition {
                from,
                to,
                trigger,
                priority,
            } => write!(f, "transition {from} -> {to} on {trigger} (priority {priority})"),
            TraceKind::Rejected { from, to } => write!(f, "rejected {from} -> {to} (cannot enter)"),
            TraceKind::EventIgnored(e) => write!(f, "event {e} ignored"),
            TraceKind::EventDropped(e) => write!(f, "event {e} dropped (queue full)"),
            TraceKind::StartFailed => write!(f, "start failed (root cannot be entered)"),
            TraceKind::Finished(s) => write!(f, "finished {s:?}"),
        }
    }
}

struct TaskSlot {
    task: Box<dyn BtTask>,
    /// `Some(Success | Failure)` once finished.
    done: Option<BtStatus>,
    /// Ticked since the last reset.
    touched: bool,
}

#[derive(Clone, Copy)]
enum CTarget {
    State(usize),
    /// The first later sibling of the owning state that can be entered.
    NextEnterable,
    /// Consume the trigger, change nothing.
    Stay,
    Succeed,
    Fail,
}

struct CTransition {
    trigger: Trigger,
    condition: Condition,
    priority: i32,
    target: CTarget,
    /// Seconds the trigger and condition must hold (0 = no hold).
    hold: f32,
    /// Consecutive ticks the trigger and condition have held (only counted when `hold > 0`).
    held: u32,
    /// `Owner -> Target`, shown in `StateTreeStatus::held`.
    label: String,
}

struct CState {
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    selection: Selection,
    enter: Condition,
    tasks: Vec<TaskSlot>,
    transitions: Vec<CTransition>,
    weight: f32,
    utility: Option<UtilityDef>,
    enter_set: Vec<(String, BbValue)>,
    fallback: Fallback,
    /// Debug: skipped by selection and entry.
    disabled: bool,
    /// Debug: pause the tree at the end of the tick that enters this state.
    pause_on_enter: bool,
}

/// What `StateTree::reload` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// Every active state still exists: the active path, tick count and event queue carry over.
    Kept,
    /// The active path no longer fits (or nothing was active): all states were exited and the
    /// next tick starts the new tree from its root.
    Restarted,
}

/// Why a state could not be entered.
enum PlanFail {
    /// A state on the way down fails its enter condition (or is debug-disabled).
    Enter,
    /// The target itself is enterable but its children offer nothing.
    Below,
}

/// A matching transition, ready to be tried.
struct Candidate {
    priority: i32,
    depth: usize,
    state: usize,
    index: usize,
}

/// A compiled state tree: resolved states and tasks plus the runtime state (active path, event
/// queue, trace). Build with [`StateTree::compile`], tick with [`StateTree::tick`] (driven each
/// `FixedUpdate` by [`crate::AiPlugin`]).
#[derive(Component)]
pub struct StateTree {
    def: StateTreeDef,
    states: Vec<CState>,
    root: usize,
    /// Active states, root first.
    active: Vec<usize>,
    status: StateTreeRunStatus,
    rng: u64,
    events: VecDeque<String>,
    ticks: u64,
    /// The `dt` the `held` counters are in, so that they stand for the same time when it changes.
    held_dt: f32,
    trace: VecDeque<TraceEvent>,
    ambient: Vec<TaskSlot>,
    paused: bool,
    pending_pause: bool,
    debug_disabled: Vec<String>,
    debug_pause: Vec<String>,
    /// The completions buffer of a tick, kept between ticks (spec 38 § 9).
    completions_buffer: Vec<Option<BtStatus>>,
    /// Matching transitions of one tick, kept between ticks.
    candidate_buf: Vec<Candidate>,
    /// The last `(from state, target state)` pair traced as rejected. A repeat of the same pair
    /// does not append another trace line.
    reject_streak: Option<(usize, usize)>,
    /// Bumped whenever the trace changes.
    trace_revision: u64,
    /// The revision the status last copied; starts unequal, so the first copy always happens.
    status_trace_revision: u64,
}

/// Whether `trigger` is satisfied by a state whose tasks have finished with `done`.
fn trigger_holds(trigger: &Trigger, done: Option<BtStatus>) -> bool {
    match trigger {
        Trigger::Tick => true,
        Trigger::Succeeded => done == Some(BtStatus::Success),
        Trigger::Failed => done == Some(BtStatus::Failure),
        Trigger::Completed => done.is_some(),
        Trigger::Event(_) => false,
    }
}

/// The ticks a `hold` of `seconds` needs at step `dt` (at least 1; a hair of slack so 0.25 s at
/// 1/60 s is 15 ticks, not 16). A step that does not advance time (`dt` zero, negative or not
/// finite) never completes a hold: `u32::MAX` ticks.
fn hold_ticks(seconds: f32, dt: f32) -> u32 {
    if dt <= 0.0 || !dt.is_finite() {
        return u32::MAX;
    }
    ((seconds / dt) - 1e-3).ceil().max(1.0) as u32
}

/// A utility child's input number: exact when the blackboard has one, else an integer rounded to
/// the nearest `f32` (a utility is a score, not an identity), else `0`.
fn utility_input(bb: &Blackboard, key: &str) -> f32 {
    bb.try_get_float(key).unwrap_or_else(|| match bb.get(key) {
        Some(BbValue::Int(_)) => bb.get_float(key),
        _ => 0.0,
    })
}

fn check_condition(c: &Condition, state: &str) -> Result<(), StateTreeError> {
    if c.depth() > MAX_CONDITION_DEPTH {
        return Err(StateTreeError::ConditionTooDeep(state.to_string()));
    }
    if !c.is_finite() {
        return Err(StateTreeError::BadValue(state.to_string()));
    }
    Ok(())
}

impl StateTree {
    /// Compiles a definition, resolving state names and task type paths against `registry`.
    ///
    /// Fails for an unsupported `format_version`, no states, too many states, empty or duplicate
    /// names, an unknown root/child/target, a state with more than one parent, a state the root
    /// cannot reach, an unregistered task, a `Next` target without a next sibling, a condition
    /// nested deeper than [`MAX_CONDITION_DEPTH`], or a non-finite number.
    pub fn compile(def: &StateTreeDef, registry: &BtTaskRegistry) -> Result<Self, StateTreeError> {
        Self::compile_with(def, registry, &|_| None)
    }

    /// Like [`StateTree::compile`], resolving `includes` through `load` (project-relative path to RON
    /// text). The merged definition (includes inlined, names prefixed) is what [`StateTree::def`]
    /// returns. Also fails with `IncludeMissing`, `IncludeCycle`, `IncludeDepth`, `HoldOnEvent` and
    /// `ParentOfRoot`. Versions 1 and 2 compile.
    pub fn compile_with(
        def: &StateTreeDef,
        registry: &BtTaskRegistry,
        load: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, StateTreeError> {
        if !(STATE_TREE_MIN_FORMAT_VERSION..=STATE_TREE_FORMAT_VERSION).contains(&def.format_version) {
            return Err(StateTreeError::UnsupportedVersion {
                found: def.format_version,
                supported: STATE_TREE_FORMAT_VERSION,
            });
        }
        let merged;
        let def = if def.has_includes() {
            merged = def.expand_includes(load)?;
            &merged
        } else {
            def
        };
        if def.states.is_empty() {
            return Err(StateTreeError::Empty);
        }
        if def.states.len() > MAX_STATES {
            return Err(StateTreeError::TooManyStates(def.states.len()));
        }
        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, s) in def.states.iter().enumerate() {
            if s.name.is_empty() {
                return Err(StateTreeError::EmptyName);
            }
            if index.insert(s.name.as_str(), i).is_some() {
                return Err(StateTreeError::DuplicateName(s.name.clone()));
            }
        }
        let lookup = |name: &str, from: &str| {
            index
                .get(name)
                .copied()
                .ok_or_else(|| StateTreeError::UnknownState {
                    name: name.to_string(),
                    from: from.to_string(),
                })
        };
        let root = lookup(&def.root, "<root>")?;

        let mut parents: Vec<Option<usize>> = vec![None; def.states.len()];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); def.states.len()];
        for (i, s) in def.states.iter().enumerate() {
            for child in &s.children {
                let c = lookup(child, &s.name)?;
                if c == root || parents[c].is_some() {
                    return Err(StateTreeError::MultipleParents(child.clone()));
                }
                parents[c] = Some(i);
                children[i].push(c);
            }
        }
        // Every state has at most one parent and the root none, so a cycle can only sit among
        // states the root does not reach.
        let mut reached = vec![false; def.states.len()];
        let mut stack = vec![root];
        while let Some(s) = stack.pop() {
            reached[s] = true;
            stack.extend(children[s].iter().copied());
        }
        if let Some(i) = reached.iter().position(|r| !r) {
            return Err(StateTreeError::Unreachable(def.states[i].name.clone()));
        }

        let mut states = Vec::with_capacity(def.states.len());
        for (i, s) in def.states.iter().enumerate() {
            check_condition(&s.enter, &s.name)?;
            let utility_ok = s
                .utility
                .as_ref()
                .is_none_or(|u| u.scale.is_finite() && u.bias.is_finite());
            if !s.weight.is_finite() || !utility_ok {
                return Err(StateTreeError::BadValue(s.name.clone()));
            }
            let mut tasks = Vec::with_capacity(s.tasks.len());
            for path in &s.tasks {
                let task = construct_task(path, &s.name, registry, load)?;
                tasks.push(TaskSlot {
                    task,
                    done: None,
                    touched: false,
                });
            }
            let mut enter_set = Vec::with_capacity(s.enter_set.len());
            for (key, literal) in &s.enter_set {
                let bad_float = matches!(literal, Literal::Float(v) if !v.is_finite());
                if key.is_empty() || bad_float {
                    return Err(StateTreeError::BadValue(s.name.clone()));
                }
                enter_set.push((key.clone(), literal.to_value()));
            }
            let next_sibling = || {
                parents[i]
                    .map(|p| &children[p])
                    .and_then(|sib| sib.iter().position(|&c| c == i).and_then(|at| sib.get(at + 1)))
                    .copied()
            };
            let mut transitions = Vec::with_capacity(s.transitions.len());
            for t in &s.transitions {
                check_condition(&t.condition, &s.name)?;
                if matches!(&t.trigger, Trigger::Event(e) if e.is_empty())
                    || !t.hold.is_finite()
                    || t.hold < 0.0
                {
                    return Err(StateTreeError::BadValue(s.name.clone()));
                }
                if t.hold > 0.0 && matches!(t.trigger, Trigger::Event(_)) {
                    return Err(StateTreeError::HoldOnEvent(s.name.clone()));
                }
                let (target, target_text) = match &t.target {
                    Target::State(name) => (CTarget::State(lookup(name, &s.name)?), name.clone()),
                    Target::Succeed => (CTarget::Succeed, "<succeed>".to_string()),
                    Target::Fail => (CTarget::Fail, "<fail>".to_string()),
                    Target::Stay => (CTarget::Stay, "<stay>".to_string()),
                    Target::Next => {
                        let next =
                            next_sibling().ok_or_else(|| StateTreeError::NoNextSibling(s.name.clone()))?;
                        (CTarget::State(next), def.states[next].name.clone())
                    }
                    Target::NextEnterable => {
                        next_sibling().ok_or_else(|| StateTreeError::NoNextSibling(s.name.clone()))?;
                        (CTarget::NextEnterable, "<next_enterable>".to_string())
                    }
                    Target::Parent => {
                        let parent =
                            parents[i].ok_or_else(|| StateTreeError::ParentOfRoot(s.name.clone()))?;
                        (CTarget::State(parent), def.states[parent].name.clone())
                    }
                };
                transitions.push(CTransition {
                    trigger: t.trigger.clone(),
                    condition: t.condition.clone(),
                    priority: t.priority,
                    target,
                    hold: t.hold,
                    held: 0,
                    label: format!("{} -> {target_text}", s.name),
                });
            }
            states.push(CState {
                name: s.name.clone(),
                parent: parents[i],
                children: children[i].clone(),
                selection: s.selection,
                enter: s.enter.clone(),
                tasks,
                transitions,
                weight: s.weight,
                utility: s.utility.clone(),
                enter_set,
                fallback: s.fallback,
                disabled: false,
                pause_on_enter: false,
            });
        }
        let mut ambient = Vec::with_capacity(def.ambient.len());
        for path in &def.ambient {
            let task = construct_task(path, "<ambient>", registry, load)?;
            ambient.push(TaskSlot {
                task,
                done: None,
                touched: false,
            });
        }
        Ok(Self {
            def: def.clone(),
            states,
            root,
            active: Vec::new(),
            status: StateTreeRunStatus::NotStarted,
            rng: def.seed,
            events: VecDeque::new(),
            ticks: 0,
            held_dt: 0.0,
            trace: VecDeque::new(),
            ambient,
            paused: false,
            pending_pause: false,
            debug_disabled: Vec::new(),
            debug_pause: Vec::new(),
            completions_buffer: Vec::new(),
            candidate_buf: Vec::new(),
            reject_streak: None,
            trace_revision: 0,
            status_trace_revision: u64::MAX,
        })
    }

    /// The definition this tree was compiled from.
    pub fn def(&self) -> &StateTreeDef {
        &self.def
    }

    /// The run status.
    pub fn status(&self) -> StateTreeRunStatus {
        self.status
    }

    /// How many times the tree has been ticked.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// The names of the active states, root first.
    pub fn active_path(&self) -> Vec<&str> {
        self.active_names().collect()
    }

    /// The names of the active states, root first, without building a list.
    pub(crate) fn active_names(&self) -> impl Iterator<Item = &str> {
        self.active.iter().map(|&s| self.states[s].name.as_str())
    }

    /// The active path as `Root/Combat/Attack` (empty when nothing is active).
    pub fn active_path_text(&self) -> String {
        self.active_path().join("/")
    }

    /// The innermost active state.
    pub fn active_leaf(&self) -> Option<&str> {
        self.active.last().map(|&s| self.states[s].name.as_str())
    }

    /// Whether the named state is active.
    pub fn is_active(&self, name: &str) -> bool {
        self.active.iter().any(|&s| self.states[s].name == name)
    }

    /// The recorded trace, oldest first (at most [`MAX_TRACE_ENTRIES`]).
    pub fn trace(&self) -> impl Iterator<Item = &TraceEvent> {
        self.trace.iter()
    }

    /// Removes and returns the recorded trace.
    pub fn take_trace(&mut self) -> Vec<TraceEvent> {
        self.trace_revision += 1;
        self.trace.drain(..).collect()
    }

    /// Queues an event for the next tick. Returns `false` (and traces the drop) when
    /// [`MAX_PENDING_EVENTS`] are already queued.
    pub fn send_event(&mut self, name: &str) -> bool {
        if self.events.len() >= MAX_PENDING_EVENTS {
            self.record(TraceKind::EventDropped(name.to_string()));
            return false;
        }
        self.events.push_back(name.to_string());
        true
    }

    fn record(&mut self, kind: TraceKind) {
        if self.trace.len() >= MAX_TRACE_ENTRIES {
            self.trace.pop_front();
        }
        self.trace.push_back(TraceEvent {
            tick: self.ticks,
            kind,
        });
        self.trace_revision += 1;
    }

    /// Whether the trace changed since the last call (the status copies its lines only then).
    pub(crate) fn take_trace_changed(&mut self) -> bool {
        let changed = self.trace_revision != self.status_trace_revision;
        self.status_trace_revision = self.trace_revision;
        changed
    }

    /// Exits every active state and returns the tree to [`StateTreeRunStatus::NotStarted`]; the
    /// next tick enters the root again. Queued events are discarded and ambient tasks are reset.
    pub fn restart(&mut self, ctx: &mut BtContext<'_>) {
        self.exit_from(0, ctx);
        self.reset_ambient(ctx);
        self.write_state_key(ctx);
        self.events.clear();
        self.status = StateTreeRunStatus::NotStarted;
    }

    /// Pauses the tree: [`StateTree::tick`] returns at once (no ambient tasks, state tasks, hold
    /// timers or events advance; queued events keep waiting; the tick count does not move).
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Lets a paused tree tick again.
    pub fn resume(&mut self) {
        self.paused = false;
        self.pending_pause = false;
    }

    /// Whether the tree is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Labels (`Owner -> Target`) of the active transitions whose `hold` counter is running.
    pub fn held(&self) -> Vec<String> {
        self.held_labels().map(str::to_owned).collect()
    }

    /// The labels of [`held`](Self::held), without building a list.
    pub(crate) fn held_labels(&self) -> impl Iterator<Item = &str> {
        self.active
            .iter()
            .flat_map(|&si| &self.states[si].transitions)
            .filter(|t| t.hold > 0.0 && t.held > 0)
            .map(|t| t.label.as_str())
    }

    /// Whether the definition has a state with this name.
    pub fn has_state(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|s| s.name == name)
    }

    /// Sets the debug lists: states in `disabled` are skipped by selection and entry (as if their
    /// `enter` failed); entering a state in `pause_on_enter` pauses the tree at the end of that
    /// tick. Unknown names are ignored. See [`StateTree::debug_lists`].
    pub fn set_debug(&mut self, disabled: &[String], pause_on_enter: &[String]) {
        if self.debug_disabled == disabled && self.debug_pause == pause_on_enter {
            return;
        }
        for s in &mut self.states {
            s.disabled = disabled.contains(&s.name);
            s.pause_on_enter = pause_on_enter.contains(&s.name);
        }
        self.debug_disabled = disabled.to_vec();
        self.debug_pause = pause_on_enter.to_vec();
    }

    /// The lists last given to [`StateTree::set_debug`]: `(disabled, pause_on_enter)`.
    pub fn debug_lists(&self) -> (&[String], &[String]) {
        (&self.debug_disabled, &self.debug_pause)
    }

    /// Replaces this tree with `next` (hot reload). When the tree is running and every active
    /// state still exists in `next` as a chain from its root, the active path, tick count, trace,
    /// seed state and event queue carry over ([`ReloadOutcome::Kept`]; the old tasks are reset
    /// and the new ones start fresh). Otherwise every state is exited and the tree returns to
    /// not-started ([`ReloadOutcome::Restarted`]). Debug lists and the pause flag carry over.
    pub fn reload(&mut self, mut next: StateTree, ctx: &mut BtContext<'_>) -> ReloadOutcome {
        let kept_path = if self.status == StateTreeRunStatus::Running {
            self.map_path(&next)
        } else {
            None
        };
        if kept_path.is_some() {
            for state in &mut self.states {
                for slot in &mut state.tasks {
                    if slot.touched {
                        slot.task.reset(ctx);
                    }
                }
            }
        } else {
            self.exit_from(0, ctx);
        }
        self.reset_ambient(ctx);
        let (disabled, pause) = (self.debug_disabled.clone(), self.debug_pause.clone());
        next.set_debug(&disabled, &pause);
        next.ticks = self.ticks;
        next.held_dt = self.held_dt;
        next.trace = std::mem::take(&mut self.trace);
        next.paused = self.paused;
        let outcome = match kept_path {
            Some(path) => {
                next.active = path;
                next.status = StateTreeRunStatus::Running;
                next.events = std::mem::take(&mut self.events);
                next.rng = self.rng;
                ReloadOutcome::Kept
            }
            None => {
                // The restarted tree has no state for these: trace them rather than lose them.
                for event in std::mem::take(&mut self.events) {
                    next.record(TraceKind::EventDropped(event));
                }
                ReloadOutcome::Restarted
            }
        };
        *self = next;
        outcome
    }

    /// The active path expressed as indices into `next`, when every active state exists there and
    /// the chain is still root, child, grandchild.
    fn map_path(&self, next: &StateTree) -> Option<Vec<usize>> {
        let mut out = Vec::with_capacity(self.active.len());
        let mut parent = None;
        for &si in &self.active {
            let ni = next.index_of(&self.states[si].name)?;
            if next.states[ni].parent != parent {
                return None;
            }
            out.push(ni);
            parent = Some(ni);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Ticks the tree once and returns its status. A paused or finished tree returns at
    /// once without counting the tick.
    pub fn tick(&mut self, ctx: &mut BtContext<'_>) -> StateTreeRunStatus {
        if self.paused {
            return self.status;
        }
        if matches!(
            self.status,
            StateTreeRunStatus::Succeeded | StateTreeRunStatus::Failed
        ) {
            // A finished tree ignores ticks and events until `restart`: the tick count stays,
            // and what it ignores is traced rather than lost.
            for event in std::mem::take(&mut self.events) {
                self.record(TraceKind::EventIgnored(event));
            }
            return self.status;
        }
        self.ticks += 1;
        self.rescale_holds(ctx.dt);
        let status = self.tick_inner(ctx);
        if self.pending_pause {
            self.pending_pause = false;
            self.paused = true;
        }
        status
    }

    /// `held` counts ticks, and a tick is `dt` seconds. When `dt` changes (a tick budget moved the agent
    /// to another cadence, ADR-2709) the counters are re-expressed in the new `dt`, so a hold still lasts
    /// its seconds. With a constant `dt` this does nothing.
    fn rescale_holds(&mut self, dt: f32) {
        if self.held_dt == dt {
            return;
        }
        if self.held_dt > 0.0 && dt > 0.0 && dt.is_finite() {
            let scale = self.held_dt / dt;
            for state in &mut self.states {
                for t in &mut state.transitions {
                    if t.held > 0 {
                        // Rounding a short hold down to 0 is right: it stands for less than one new tick.
                        t.held = (t.held as f32 * scale).round() as u32;
                    }
                }
            }
        }
        self.held_dt = dt;
    }

    fn tick_inner(&mut self, ctx: &mut BtContext<'_>) -> StateTreeRunStatus {
        match self.status {
            StateTreeRunStatus::Succeeded | StateTreeRunStatus::Failed => {
                // A finished tree runs no transitions: trace what it ignores.
                for event in std::mem::take(&mut self.events) {
                    self.record(TraceKind::EventIgnored(event));
                }
                return self.status;
            }
            StateTreeRunStatus::NotStarted | StateTreeRunStatus::Running => {}
        }
        self.tick_ambient(ctx);
        if self.status == StateTreeRunStatus::NotStarted && !self.start(ctx) {
            self.reset_ambient(ctx);
            return self.status;
        }

        let events: Vec<String> = self.events.drain(..).collect();
        for event in events {
            if self.status != StateTreeRunStatus::Running {
                // An earlier event finished the tree: this one is not delivered.
                self.record(TraceKind::EventIgnored(event));
                continue;
            }
            let completions = self.completions();
            let mut candidates = std::mem::take(&mut self.candidate_buf);
            self.collect_candidates(Some(&event), &completions, ctx, &mut candidates);
            let label = format!("event:{event}");
            let taken = !candidates.is_empty() && self.take_first(&candidates, &label, ctx);
            self.candidate_buf = candidates;
            if !taken {
                self.record(TraceKind::EventIgnored(event));
            }
        }
        if self.status != StateTreeRunStatus::Running {
            return self.status;
        }

        self.tick_tasks(ctx);
        let mut completions = std::mem::take(&mut self.completions_buffer);
        self.completions_into(&mut completions);
        self.advance_holds(&completions, ctx);
        let mut candidates = std::mem::take(&mut self.candidate_buf);
        self.collect_candidates(None, &completions, ctx, &mut candidates);
        let taken = self.take_first(&candidates, "", ctx);
        self.candidate_buf = candidates;
        if !taken && let Some(Some(result)) = completions.first() {
            let status = if *result == BtStatus::Success {
                StateTreeRunStatus::Succeeded
            } else {
                StateTreeRunStatus::Failed
            };
            self.finish(status, ctx);
        }
        self.completions_buffer = completions;
        self.status
    }

    fn tick_ambient(&mut self, ctx: &mut BtContext<'_>) {
        for slot in &mut self.ambient {
            slot.touched = true;
            // The result is ignored: ambient tasks only publish data.
            let _ = slot.task.tick(ctx);
        }
    }

    fn reset_ambient(&mut self, ctx: &mut BtContext<'_>) {
        for slot in &mut self.ambient {
            if slot.touched {
                slot.task.reset(ctx);
            }
            slot.touched = false;
        }
    }

    /// Advances the `hold` counter of every active transition whose trigger and condition hold,
    /// and clears the counters of the others.
    fn advance_holds(&mut self, completions: &[Option<BtStatus>], ctx: &BtContext<'_>) {
        let empty = Blackboard::default();
        let bb = ctx.blackboard().unwrap_or(&empty);
        for (depth, &si) in self.active.iter().enumerate() {
            let done = completions.get(depth).copied().flatten();
            for t in &mut self.states[si].transitions {
                if t.hold <= 0.0 {
                    continue;
                }
                if trigger_holds(&t.trigger, done) && t.condition.eval(bb) {
                    t.held = t.held.saturating_add(1);
                } else {
                    t.held = 0;
                }
            }
        }
    }

    fn start(&mut self, ctx: &mut BtContext<'_>) -> bool {
        let plan = {
            let empty = Blackboard::default();
            let bb = ctx.blackboard().unwrap_or(&empty);
            self.plan_entry(self.root, bb)
        };
        match plan {
            Ok((keep, enter)) => {
                self.apply(keep, enter, ctx);
                self.status = StateTreeRunStatus::Running;
                true
            }
            Err(_) => {
                self.status = StateTreeRunStatus::Failed;
                // Nothing is active: the key must not keep a stale leaf name.
                self.write_state_key(ctx);
                self.record(TraceKind::StartFailed);
                false
            }
        }
    }

    /// The completion result of one state's own tasks: `None` while any is running or when the
    /// state has no tasks.
    fn task_completion(state: &CState) -> Option<BtStatus> {
        if state.tasks.is_empty() {
            return None;
        }
        if state.tasks.iter().any(|t| t.done == Some(BtStatus::Failure)) {
            return Some(BtStatus::Failure);
        }
        state
            .tasks
            .iter()
            .all(|t| t.done == Some(BtStatus::Success))
            .then_some(BtStatus::Success)
    }

    /// Completion per active state (parallel to the active path). A state with tasks completes
    /// when they finish; a state without tasks takes its active child's completion.
    fn completions(&self) -> Vec<Option<BtStatus>> {
        let mut out = Vec::new();
        self.completions_into(&mut out);
        out
    }

    /// [`completions`](Self::completions) into a buffer the caller keeps.
    fn completions_into(&self, out: &mut Vec<Option<BtStatus>>) {
        let n = self.active.len();
        out.clear();
        out.resize(n, None);
        for d in (0..n).rev() {
            let state = &self.states[self.active[d]];
            out[d] = if state.tasks.is_empty() {
                out.get(d + 1).copied().flatten()
            } else {
                Self::task_completion(state)
            };
        }
    }

    fn tick_tasks(&mut self, ctx: &mut BtContext<'_>) {
        // Tasks never touch the active path, so it is moved out for the loop.
        let active = std::mem::take(&mut self.active);
        for &si in &active {
            if Self::task_completion(&self.states[si]) == Some(BtStatus::Failure) {
                continue;
            }
            for slot in &mut self.states[si].tasks {
                if slot.done.is_some() {
                    continue;
                }
                slot.touched = true;
                let result = slot.task.tick(ctx);
                if result != BtStatus::Running {
                    slot.done = Some(result);
                    if result == BtStatus::Failure {
                        break;
                    }
                }
            }
        }
        self.active = active;
    }

    /// Every matching transition of the active states, best first: higher priority, then deeper
    /// state, then declaration order. `event` is `Some` when delivering an event (only event
    /// transitions match) and `None` for the tick pool (tick and completion transitions).
    fn collect_candidates(
        &self,
        event: Option<&str>,
        completions: &[Option<BtStatus>],
        ctx: &BtContext<'_>,
        out: &mut Vec<Candidate>,
    ) {
        out.clear();
        let empty = Blackboard::default();
        let bb = ctx.blackboard().unwrap_or(&empty);
        for (depth, &si) in self.active.iter().enumerate() {
            let done = completions.get(depth).copied().flatten();
            for (index, t) in self.states[si].transitions.iter().enumerate() {
                let matches = match (&t.trigger, event) {
                    (Trigger::Event(name), Some(e)) => name == e,
                    (_, Some(_)) | (Trigger::Event(_), None) => false,
                    // A held transition matches from the tick its counter is reached while the
                    // trigger and condition keep holding: one rejected (target not enterable) or
                    // outranked on that tick still fires later (zai L5, ADR-1809).
                    (trigger, None) => {
                        trigger_holds(trigger, done)
                            && (t.hold <= 0.0 || t.held >= hold_ticks(t.hold, ctx.dt))
                    }
                };
                if matches && t.condition.eval(bb) {
                    out.push(Candidate {
                        priority: t.priority,
                        depth,
                        state: si,
                        index,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(b.depth.cmp(&a.depth))
                .then(a.index.cmp(&b.index))
        });
    }

    /// Records a rejected enter at most once per `(from, to)` streak.
    fn reject_once(&mut self, from: usize, to: usize) {
        if self.reject_streak == Some((from, to)) {
            return;
        }
        self.reject_streak = Some((from, to));
        self.record(TraceKind::Rejected {
            from: self.states[from].name.clone(),
            to: self.states[to].name.clone(),
        });
    }

    /// Takes the first candidate that can be entered. Returns whether one was taken.
    fn take_first(&mut self, candidates: &[Candidate], label: &str, ctx: &mut BtContext<'_>) -> bool {
        if let Some((from, to)) = self.reject_streak {
            let still = candidates.iter().any(|candidate| {
                candidate.state == from
                    && matches!(
                        self.states[candidate.state].transitions.get(candidate.index).map(|t| t.target),
                        Some(CTarget::State(target)) if target == to
                    )
            });
            if !still {
                self.reject_streak = None;
            }
        }
        for c in candidates {
            let target = self.states[c.state].transitions[c.index].target;
            // A state that cannot be entered, and will not try a sibling, is rejected before any
            // name or plan is built. The same pair on later ticks adds no trace line.
            if let CTarget::State(to) = target {
                let empty = Blackboard::default();
                let bb = ctx.blackboard().unwrap_or(&empty);
                if !self.can_enter(to, bb) && self.states[to].fallback != Fallback::NextEnterable {
                    self.reject_once(c.state, to);
                    continue;
                }
            }
            let (trigger, priority) = {
                let t = &self.states[c.state].transitions[c.index];
                (t.trigger.clone(), t.priority)
            };
            let from = self.states[c.state].name.clone();
            let trigger_label = if label.is_empty() {
                match trigger {
                    Trigger::Tick => "tick".to_string(),
                    Trigger::Succeeded => "succeeded".to_string(),
                    Trigger::Failed => "failed".to_string(),
                    Trigger::Completed => "completed".to_string(),
                    Trigger::Event(e) => format!("event:{e}"),
                }
            } else {
                label.to_string()
            };
            let planned = match target {
                CTarget::Succeed | CTarget::Fail => {
                    let (status, to) = if matches!(target, CTarget::Succeed) {
                        (StateTreeRunStatus::Succeeded, "<succeed>")
                    } else {
                        (StateTreeRunStatus::Failed, "<fail>")
                    };
                    self.record(TraceKind::Transition {
                        from,
                        to: to.to_string(),
                        trigger: trigger_label,
                        priority,
                    });
                    self.finish(status, ctx);
                    self.reject_streak = None;
                    return true;
                }
                CTarget::Stay => {
                    // Consumes the trigger; only an event is worth a trace line.
                    if !label.is_empty() {
                        self.record(TraceKind::Transition {
                            from,
                            to: "<stay>".to_string(),
                            trigger: trigger_label,
                            priority,
                        });
                    }
                    self.reject_streak = None;
                    return true;
                }
                CTarget::State(to) => {
                    let empty = Blackboard::default();
                    let bb = ctx.blackboard().unwrap_or(&empty);
                    match self.plan_entry(to, bb) {
                        Ok((keep, enter)) => Some((to, keep, enter)),
                        Err(PlanFail::Below) if self.states[to].fallback == Fallback::NextEnterable => {
                            self.first_enterable_after(to, bb)
                        }
                        Err(_) => None,
                    }
                }
                CTarget::NextEnterable => {
                    let empty = Blackboard::default();
                    let bb = ctx.blackboard().unwrap_or(&empty);
                    self.first_enterable_after(c.state, bb)
                }
            };
            match planned {
                Some((to, keep, enter)) => {
                    let to_name = self.states[to].name.clone();
                    self.record(TraceKind::Transition {
                        from,
                        to: to_name,
                        trigger: trigger_label,
                        priority,
                    });
                    self.apply(keep, enter, ctx);
                    self.reject_streak = None;
                    return true;
                }
                None => {
                    let to = match target {
                        CTarget::State(to) => self.states[to].name.clone(),
                        _ => "<next_enterable>".to_string(),
                    };
                    self.record(TraceKind::Rejected { from, to });
                }
            }
        }
        false
    }

    /// Plans entering the first later sibling of `state` that can be entered: `(sibling, keep,
    /// states to enter)`.
    fn first_enterable_after(&mut self, state: usize, bb: &Blackboard) -> Option<(usize, usize, Vec<usize>)> {
        let parent = self.states[state].parent?;
        let siblings: Vec<usize> = {
            let all = &self.states[parent].children;
            let at = all.iter().position(|&c| c == state)?;
            all[at + 1..].to_vec()
        };
        for sibling in siblings {
            if let Ok((keep, enter)) = self.plan_entry(sibling, bb) {
                return Some((sibling, keep, enter));
            }
        }
        None
    }

    fn can_enter(&self, state: usize, bb: &Blackboard) -> bool {
        let s = &self.states[state];
        !s.disabled && s.enter.eval(bb)
    }

    /// Plans entering `target`: how many active states stay, and the states to enter (target's
    /// ancestors that are not active, the target, then the selected descent). Fails when an enter
    /// condition fails or a state's children offer nothing to enter.
    fn plan_entry(&mut self, target: usize, bb: &Blackboard) -> Result<(usize, Vec<usize>), PlanFail> {
        let mut chain = vec![target];
        let mut top = target;
        while let Some(parent) = self.states[top].parent {
            chain.push(parent);
            top = parent;
        }
        chain.reverse();
        // The target itself is always re-entered, even when it is already active.
        let mut keep = 0;
        while keep + 1 < chain.len() && self.active.get(keep) == Some(&chain[keep]) {
            keep += 1;
        }
        let mut enter: Vec<usize> = chain[keep..].to_vec();
        if enter.iter().any(|&s| !self.can_enter(s, bb)) {
            return Err(PlanFail::Enter);
        }
        let mut cur = target;
        while !self.states[cur].children.is_empty() {
            cur = self.pick_child(cur, bb).ok_or(PlanFail::Below)?;
            enter.push(cur);
        }
        Ok((keep, enter))
    }

    fn next_unit(&mut self) -> f32 {
        // SplitMix64 (Steele, Lea, Flood 2014); the top 24 bits make an f32 in [0, 1).
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 40) as f32 / 16_777_216.0
    }

    fn pick_child(&mut self, parent: usize, bb: &Blackboard) -> Option<usize> {
        let enterable: Vec<usize> = self.states[parent]
            .children
            .iter()
            .copied()
            .filter(|&c| self.can_enter(c, bb))
            .collect();
        match self.states[parent].selection {
            Selection::First => enterable.first().copied(),
            Selection::Utility => {
                let mut best: Option<(usize, f32)> = None;
                for &c in &enterable {
                    let score = match &self.states[c].utility {
                        Some(u) => u.bias + u.scale * utility_input(bb, &u.key),
                        None => 0.0,
                    };
                    if score.is_finite() && best.is_none_or(|(_, b)| score > b) {
                        best = Some((c, score));
                    }
                }
                best.map(|(c, _)| c)
            }
            Selection::Weighted => {
                let weight = |s: &CState| if s.weight > 0.0 { s.weight } else { 0.0 };
                // Summed in f64: two finite f32 weights near `f32::MAX` overflow an f32 sum.
                let total: f64 = enterable
                    .iter()
                    .map(|&c| f64::from(weight(&self.states[c])))
                    .sum();
                if total <= 0.0 || !total.is_finite() {
                    return None;
                }
                let mut roll = f64::from(self.next_unit()) * total;
                let mut last = None;
                for &c in &enterable {
                    let w = f64::from(weight(&self.states[c]));
                    if w <= 0.0 {
                        continue;
                    }
                    last = Some(c);
                    if roll < w {
                        return Some(c);
                    }
                    roll -= w;
                }
                last
            }
        }
    }

    /// Exits the active states from depth `keep` down (leaf first).
    fn exit_from(&mut self, keep: usize, ctx: &mut BtContext<'_>) {
        while self.active.len() > keep {
            let Some(si) = self.active.pop() else { break };
            for slot in &mut self.states[si].tasks {
                if slot.touched {
                    slot.task.reset(ctx);
                }
                slot.touched = false;
                slot.done = None;
            }
            for t in &mut self.states[si].transitions {
                t.held = 0;
            }
            let name = self.states[si].name.clone();
            self.record(TraceKind::Exited(name));
        }
    }

    fn apply(&mut self, keep: usize, enter: Vec<usize>, ctx: &mut BtContext<'_>) {
        self.exit_from(keep, ctx);
        for si in enter {
            self.active.push(si);
            let name = self.states[si].name.clone();
            self.record(TraceKind::Entered(name));
            // `enter_set` lands before the state's tasks first tick, in key order.
            if !self.states[si].enter_set.is_empty()
                && let Some(mut bb) = ctx.blackboard_mut()
            {
                for (key, value) in &self.states[si].enter_set {
                    bb.insert(key.clone(), value.clone());
                }
            }
            if self.states[si].pause_on_enter {
                self.pending_pause = true;
            }
        }
        self.write_state_key(ctx);
    }

    /// Writes the active leaf (empty when nothing is active) to `state_key`, when there is one.
    fn write_state_key(&self, ctx: &mut BtContext<'_>) {
        if !self.def.state_key.is_empty() {
            let leaf = self.active_leaf().unwrap_or("").to_string();
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert(self.def.state_key.clone(), leaf);
            }
        }
    }

    fn finish(&mut self, status: StateTreeRunStatus, ctx: &mut BtContext<'_>) {
        self.exit_from(0, ctx);
        self.reset_ambient(ctx);
        // The active path is empty now: the key must not keep naming the last leaf.
        self.write_state_key(ctx);
        self.events.clear();
        self.status = status;
        self.record(TraceKind::Finished(status));
    }
}
