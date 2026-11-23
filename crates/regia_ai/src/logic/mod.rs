// SPDX-License-Identifier: MPL-2.0
//! Logic graphs (`docs/specs/102-logic-graph.md`, phase 1): designer-facing event → condition →
//! action rules, authored as `.logic.ron` ([`LogicGraphDef`], format version 1) and compiled into a
//! small rule interpreter ([`LogicGraph`]). "Logic Graph" is the working product name (owner
//! decision 1 is open).
//!
//! # How the pieces fit
//!
//! An entity carries one [`LogicGraph`] (compiled against the [`crate::BtTaskRegistry`]), usually a
//! [`crate::Blackboard`] (the entity scope; the graph's `local/` scope lives inside the graph) and a
//! [`LogicInbox`] that gameplay code, other graphs (`logic::EmitEvent`) and the agent (`logic.emit`)
//! push named events to. Each `FixedUpdate` [`crate::AiPlugin`] ticks every graph after the state
//! trees, sharing the [`crate::AiTickBudget`], and writes a reflected [`LogicGraphStatus`] next to it.
//! [`LogicAuthority`] decides whether a peer runs sim actions (always, offline).
//!
//! See `book/agents/logic-graph.md` and `docs/formats/logic-ron.md`.

#[cfg(test)]
mod debug_tests;
mod def;
#[cfg(test)]
mod p3_tests;
mod reload;
mod runtime;
#[cfg(test)]
mod tests;
#[cfg(all(test, debug_assertions))]
mod trace_tests;
#[cfg(test)]
mod v2_tests;

pub use def::{
    ActionDef, Binding, EntityVarDef, LOGIC_FORMAT_VERSION, LocalDef, LogicEvent, LogicGraphDef,
    LogicGraphError, MAX_ACTIONS_PER_RULE, MAX_LOCALS, MAX_RULES, MAX_SET_ENTRIES, MAX_WORLD_BOARDS, RuleDef,
    WorldVarDef, input_action_event, logic_migration_sources, logic_schema_fingerprint, net_message_event,
    sequence_marker_event,
};
pub use reload::{LogicReloadRecord, LogicReloads, MAX_LOGIC_RELOAD_LOG};
pub use runtime::{
    BUILTIN_TASKS, LogicGraph, LogicReloadOutcome, MAX_PENDING_EVENTS, MAX_STATUS_LOCALS, MAX_TRACE_ENTRIES,
};
// The debugger's scope reads (ADR-8021).
pub use runtime::LogicScopeView;
// Logic authored on prefab and level entities (P-165, ADR-8566).
mod graph_ref;
pub(crate) use graph_ref::attach_logic_graph_refs;
pub use graph_ref::{
    LOGIC_GRAPH_REF_FORMAT_VERSION, LogicGraphRef, LogicGraphRefError, LogicGraphRefLoads,
    LogicGraphRefRecord, LogicLocalOverride, LogicOverrideValue, MAX_LOGIC_GRAPH_REF_LOG,
    logic_graph_ref_migration_sources, logic_graph_ref_schema_fingerprint,
};

use regia_app::prelude::*;
use regia_scene::layers::LayerParked;

use std::collections::BTreeMap;

use crate::blackboard::{BbValue, Blackboard, assign_value};
use crate::task::BtContext;

/// Named events waiting for the entity's [`LogicGraph`]. The tick system moves everything pushed
/// here into the graph at the start of its tick, in order (at most `MAX_PENDING_EVENTS` per tick; the
/// oldest overflow is dropped with an `inbox_overflow` trace line). Events pushed during a tick are
/// delivered on the next one. Reflected, so an agent can also append with `scene.set_property`.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct LogicInbox {
    /// Event names, oldest first.
    pub events: Vec<String>,
}

impl LogicInbox {
    /// Appends an event.
    pub fn push(&mut self, name: impl Into<String>) {
        self.events.push(name.into());
    }
}

/// The named world blackboards graphs share (spec 102 § 5, phase 3): `world/<name>/key` in a graph
/// that declares `<name>` in its `world:` list reads and writes `boards[name]`. A graph creates its
/// declared boards on its first tick; the boards live as long as the world (a level), not a graph.
/// Reflected, so an agent can read it as a resource; `logic.world` reads and writes it too.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct LogicWorldBlackboards {
    /// Board name → board.
    pub boards: BTreeMap<String, Blackboard>,
}

impl LogicWorldBlackboards {
    /// The board `name`, if any graph declared it or a host created it.
    pub fn get(&self, name: &str) -> Option<&Blackboard> {
        self.boards.get(name)
    }

    /// The board `name`, created empty when missing (a host seeding a value before any graph runs).
    pub fn board_mut(&mut self, name: &str) -> &mut Blackboard {
        self.boards.entry(name.to_string()).or_default()
    }
}

/// Whether this peer is the authority for a graph entity (spec 102 § 8). A peer without authority
/// still ticks its graphs but skips sim actions (the built-in actions and every task not marked
/// presentation with `BtTaskRegistry::mark_presentation`). Without the resource, or with the
/// default, every graph is authoritative (the offline case); the umbrella crate's net wiring
/// installs a check built on `NetRole`.
#[derive(Resource, Clone, Copy)]
pub struct LogicAuthority(pub fn(Entity, &World) -> bool);

impl Default for LogicAuthority {
    fn default() -> Self {
        Self(|_, _| true)
    }
}

impl std::fmt::Debug for LogicAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LogicAuthority(..)")
    }
}

/// Development debug controls for a [`LogicGraph`] (spec 102 § 9, phase 4): rules named in
/// `disabled` never match; a fire of a rule in `pause_on_rule`, or an inbox event named in
/// `pause_on_event`, pauses the graph at the end of that tick (resume with `logic.control resume`,
/// advance one tick with `step`). Applied by the tick system; never part of the asset. Compiled out
/// of shipping builds (`cfg(debug_assertions)`), the `StateTreeDebug` pattern.
#[cfg(debug_assertions)]
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct LogicGraphDebug {
    /// Rule names that pause the graph at the end of the tick they fire in.
    pub pause_on_rule: Vec<String>,
    /// Inbox event names that pause the graph at the end of the tick they arrive in.
    pub pause_on_event: Vec<String>,
    /// Rule names that never match.
    pub disabled: Vec<String>,
}

/// One rule of a [`LogicGraphStatus`].
#[derive(Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Default)]
pub struct LogicRuleStatus {
    /// The rule name.
    pub name: String,
    /// How many times its actions started.
    pub fires: u64,
    /// How many times its event and condition matched (fires plus matches during a cooldown or
    /// while its chain was busy).
    pub matches: u64,
    /// The graph tick of the last fire (`None`: never).
    pub last_fired_tick: Option<u64>,
    /// Ticks until it may fire again.
    pub cooldown_remaining: u64,
    /// Its action chain is suspended on a running task.
    pub chain_busy: bool,
}

/// One local of a [`LogicGraphStatus`].
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct LogicLocalStatus {
    /// The local name (without `local/`).
    pub name: String,
    /// Its value.
    pub value: BbValue,
}

/// Which trace records a [`LogicGraphStatus::trace`] holds: runtime bookkeeping of the
/// change-checked refresh (the graph's [`LogicGraph::trace_id`] and [`LogicGraph::trace_total`] when
/// the lines were written). Not reflected or saved, never part of equality, and its `Debug` prints
/// no numbers (the identity differs from run to run).
#[derive(Clone, Copy, Default)]
pub struct LogicTraceMirror {
    id: u64,
    total: u64,
}

impl PartialEq for LogicTraceMirror {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for LogicTraceMirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LogicTraceMirror")
    }
}

/// Spare capacity a status trace line keeps, so a line that grows by a digit (`tick 999` to
/// `tick 1000`) still fits its buffer.
const LINE_HEADROOM: usize = 16;

/// The starting capacity of a status trace line: a typical line (`tick 1234: rule_matched <rule> on
/// timer:<rule> (cooldown 9)`) fits, so the buffers converge within the first ticks instead of
/// growing whenever a rotated buffer first meets a longer line.
const LINE_CAPACITY: usize = 96;

/// A reflected, read-only view of a logic graph after its last tick (spec 102 § 9), written next to
/// the [`LogicGraph`] by [`crate::AiPlugin`]. The refresh touches only what changed and overwrites
/// in place, so a steady graph reuses its strings and lists and allocates nothing, even while rules
/// fire and new trace lines arrive (spec 102 § 13 P4.3, ADR-7282).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct LogicGraphStatus {
    /// Started and not paused.
    pub running: bool,
    /// Paused (`logic.control pause`).
    pub paused: bool,
    /// Graph ticks since the last start.
    pub ticks: u64,
    /// Rule evaluations in the last tick.
    pub evaluations: u32,
    /// Per rule, in declaration order.
    pub rules: Vec<LogicRuleStatus>,
    /// Locals in key order (at most `MAX_STATUS_LOCALS`).
    pub locals: Vec<LogicLocalStatus>,
    /// The declared world boards' entries as `<board>/<key>`, boards in declaration order, keys in
    /// key order (at most `MAX_STATUS_LOCALS` in all).
    pub world: Vec<LogicLocalStatus>,
    /// The recent trace, oldest first.
    pub trace: Vec<String>,
    /// Which graph trace records `trace` holds (bookkeeping of the in-place refresh; not reflected).
    #[reflect(ignore)]
    pub trace_mirror: LogicTraceMirror,
}

impl LogicGraphStatus {
    /// Snapshots `graph` (without its world boards: see [`LogicGraphStatus::of_in`]).
    pub fn of(graph: &LogicGraph) -> Self {
        let mut status = Self::default();
        status.copy_from(graph, None);
        status
    }

    /// Snapshots `graph` with the values of its declared world boards in `boards`.
    pub fn of_in(graph: &LogicGraph, boards: Option<&LogicWorldBlackboards>) -> Self {
        let mut status = Self::default();
        status.copy_from(graph, boards);
        status
    }

    /// Brings this status up to date with `graph`, touching only what changed: values are
    /// overwritten in place and only the trace lines added since the last refresh are formatted
    /// (into the buffers of the lines that dropped out).
    pub(crate) fn refresh(&mut self, graph: &LogicGraph, boards: Option<&LogicWorldBlackboards>) {
        self.copy_from(graph, boards);
    }

    fn copy_from(&mut self, graph: &LogicGraph, boards: Option<&LogicWorldBlackboards>) {
        self.running = graph.is_started() && !graph.is_paused();
        self.paused = graph.is_paused();
        self.ticks = graph.ticks();
        self.evaluations = graph.evaluations_last_tick();
        let same_rules = self.rules.len() == graph.rule_stats().count()
            && graph.rule_stats().zip(&self.rules).all(|(r, s)| r.0 == s.name);
        if !same_rules {
            self.rules = graph
                .rule_stats()
                .map(|r| LogicRuleStatus {
                    name: r.0.to_string(),
                    ..Default::default()
                })
                .collect();
        }
        for ((_, fires, matches, last, cooldown, busy), s) in graph.rule_stats().zip(&mut self.rules) {
            s.fires = fires;
            s.matches = matches;
            s.last_fired_tick = last;
            s.cooldown_remaining = cooldown;
            s.chain_busy = busy;
        }
        let locals = graph.locals().values.iter().take(MAX_STATUS_LOCALS);
        let same_names = self.locals.len() == locals.len()
            && locals.clone().zip(&self.locals).all(|((k, _), s)| *k == s.name);
        if same_names {
            for ((_, v), s) in locals.zip(&mut self.locals) {
                if s.value != *v {
                    assign_value(&mut s.value, v);
                }
            }
        } else {
            self.locals = locals
                .map(|(k, v)| LogicLocalStatus {
                    name: k.clone(),
                    value: v.clone(),
                })
                .collect();
        }
        // The world boards, change-checked like the locals.
        let world = graph
            .def()
            .world
            .iter()
            .filter_map(|name| Some((name, boards?.get(name)?)))
            .flat_map(|(name, board)| board.values.iter().map(move |(k, v)| (name, k, v)))
            .take(MAX_STATUS_LOCALS);
        let same_world = self.world.len() == world.clone().count()
            && world.clone().zip(&self.world).all(|((b, k, _), s)| {
                s.name.len() == b.len() + 1 + k.len()
                    && s.name.starts_with(b.as_str())
                    && s.name.ends_with(k.as_str())
                    && s.name.as_bytes().get(b.len()) == Some(&b'/')
            });
        if same_world {
            for ((_, _, v), s) in world.zip(&mut self.world) {
                if s.value != *v {
                    assign_value(&mut s.value, v);
                }
            }
        } else {
            self.world = world
                .map(|(b, k, v)| LogicLocalStatus {
                    name: format!("{b}/{k}"),
                    value: v.clone(),
                })
                .collect();
        }
        self.sync_trace(graph);
    }

    /// Mirrors the graph's trace into `trace`. The lines it already holds of the same trace stay;
    /// the ones that dropped out of the graph's ring rotate to the end and take the new lines'
    /// text, so a full ring costs one format per new record and no allocation.
    fn sync_trace(&mut self, graph: &LogicGraph) {
        let (id, total, len) = (graph.trace_id(), graph.trace_total(), graph.trace_len());
        let mirror = self.trace_mirror;
        let held = self.trace.len();
        if mirror.id == id && mirror.total == total && held == len {
            return;
        }
        // The status holds records `[mirror.total - held, mirror.total)`; the graph has
        // `[total - len, total)`. Keep the overlap, if this is the same trace and it moved forward.
        let mut kept = 0;
        if mirror.id == id && mirror.total <= total && held as u64 <= mirror.total && len as u64 <= total {
            let have_from = mirror.total - held as u64;
            let want_from = total - len as u64;
            if want_from >= have_from {
                let dropped = usize::try_from(want_from - have_from)
                    .unwrap_or(usize::MAX)
                    .min(held);
                self.trace.rotate_left(dropped);
                kept = (held - dropped).min(len);
            }
        }
        self.trace.truncate(len);
        self.trace
            .resize_with(len, || String::with_capacity(LINE_CAPACITY));
        for (i, line) in self.trace.iter_mut().enumerate().skip(kept) {
            line.clear();
            graph.write_trace_line(i, line);
            if line.capacity() - line.len() < LINE_HEADROOM {
                line.reserve(LINE_HEADROOM);
            }
        }
        self.trace_mirror = LogicTraceMirror { id, total };
    }
}

/// `FixedUpdate` (exclusive): ticks every [`LogicGraph`] with take-run-reinsert semantics, after
/// the state trees, moving each entity's [`LogicInbox`] into its graph first and refreshing its
/// [`LogicGraphStatus`] afterwards.
pub(crate) fn run_logic_graphs(
    world: &mut World,
    query: &mut QueryState<Entity, (With<LogicGraph>, Without<LayerParked>)>,
    mut entities: Local<Vec<Entity>>,
) {
    tick_logic_graphs_with(world, query, &mut entities);
}

/// One pass of [`run_logic_graphs`] with its own query and list, for a test that drives it by hand.
#[cfg(test)]
pub(crate) fn tick_logic_graphs(world: &mut World) {
    let mut query = world.query_filtered::<Entity, (With<LogicGraph>, Without<LayerParked>)>();
    tick_logic_graphs_with(world, &mut query, &mut Vec::new());
}

#[tracing::instrument(skip_all, target = "regia_ai", name = "logic_graph_tick")]
fn tick_logic_graphs_with(
    world: &mut World,
    query: &mut QueryState<Entity, (With<LogicGraph>, Without<LayerParked>)>,
    entities: &mut Vec<Entity>,
) {
    let step_dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    entities.clear();
    entities.extend(query.iter(world));
    crate::agent_order::<LogicGraph>(world, entities);
    // With an `AiTickBudget` on, `entities` holds this step's graphs and each gets its accumulated time.
    let budget_dts = crate::tick_budget::plan_logic_graphs(world, entities);
    // Graphs compiled from new text wait in `LogicReloads` and swap in at the start of their
    // entity's tick, never in the middle of one (spec 102 § 7).
    let reloads_waiting = world
        .get_resource::<LogicReloads>()
        .is_some_and(|reloads| reloads.pending() > 0);
    let mut records = Vec::new();
    for (index, &entity) in entities.iter().enumerate() {
        let Ok(mut host) = world.get_entity_mut(entity) else {
            continue;
        };
        let Some(mut graph) = host.take::<LogicGraph>() else {
            continue;
        };
        let dt = budget_dts.get(index).copied().unwrap_or(step_dt);
        let steps = if step_dt > 0.0 {
            ((dt / step_dt).round() as u64).max(1)
        } else {
            1
        };
        #[cfg(debug_assertions)]
        {
            match host.get::<LogicGraphDebug>() {
                Some(debug) => graph.set_debug(&debug.disabled, &debug.pause_on_rule, &debug.pause_on_event),
                None => graph.set_debug(&[], &[], &[]),
            }
        }
        if (!graph.is_paused() || graph.is_stepping())
            && let Some(mut inbox) = host.get_mut::<LogicInbox>()
        {
            for event in inbox.events.drain(..) {
                graph.send_event(event);
            }
        }
        let reload = if reloads_waiting {
            world
                .get_resource_mut::<LogicReloads>()
                .and_then(|mut reloads| reloads.take_for(entity))
        } else {
            None
        };
        let mut ctx = BtContext { world, entity, dt };
        if let Some((source, next)) = reload {
            let outcome = graph.reload(next, &mut ctx);
            records.push(LogicReloadRecord {
                entity,
                source,
                result: Ok(outcome),
            });
        }
        graph.tick(&mut ctx, steps);
        // A task may have despawned the entity or installed another graph on it: then this one is
        // dropped.
        // The world boards are lent out while the status refreshes (only for a graph that declares
        // any), so the status can read them next to its entity.
        let boards = if graph.def().world.is_empty() {
            None
        } else {
            world.remove_resource::<LogicWorldBlackboards>()
        };
        if let Ok(mut host) = world.get_entity_mut(entity)
            && !host.contains::<LogicGraph>()
        {
            match host.get_mut::<LogicGraphStatus>() {
                Some(mut status) => status.refresh(&graph, boards.as_ref()),
                None => {
                    host.insert(LogicGraphStatus::of_in(&graph, boards.as_ref()));
                }
            }
            host.insert(graph);
        }
        if let Some(boards) = boards {
            world.insert_resource(boards);
        }
    }
    crate::tick_budget::recycle_dts(world, budget_dts);
    if (reloads_waiting || !records.is_empty())
        && let Some(mut reloads) = world.remove_resource::<LogicReloads>()
    {
        for record in records {
            reloads.push_record(record);
        }
        // A graph queued for a despawned entity is dropped; one for a graph that did not tick
        // this step (deferred by the budget, parked) waits.
        reloads.retain_live(world);
        world.insert_resource(reloads);
    }
}
