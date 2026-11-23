// SPDX-License-Identifier: MPL-2.0
//! State trees (`docs/specs/22-tier2.md` § T2.2, ADR-1565): hierarchical states with enter
//! conditions, tasks that run while a state is active, prioritized transitions on tick, event or
//! completion, and utility or weighted choice among children. Authored as `.statetree.ron`
//! ([`StateTreeDef`], format version 2; version 1 files still load).
//!
//! # How the pieces fit
//!
//! An agent entity carries a [`StateTree`] (compiled from a [`StateTreeDef`] against the
//! [`crate::BtTaskRegistry`]), a [`crate::Blackboard`] that conditions and utility scores read, and
//! optionally a [`StateTreeInbox`] that agents and scripts append event names to. Tasks are the
//! same [`crate::BtTask`] types behaviour trees use, so one task library serves both. Each
//! `FixedUpdate` [`crate::AiPlugin`] ticks every tree and writes a reflected [`StateTreeStatus`]
//! (run status, active path, recent trace) next to it.
//!
//! See `book/agents/state-tree.md` and `docs/formats/statetree-ron.md`.

mod def;
mod reload;
mod runtime;
#[cfg(test)]
mod tests;

pub(crate) use def::version_number_span;
pub use def::{
    CompareOp, Condition, Fallback, IncludeDef, Literal, MAX_CONDITION_DEPTH, MAX_INCLUDE_DEPTH, MAX_STATES,
    STATE_TREE_FORMAT_VERSION, STATE_TREE_MIN_FORMAT_VERSION, Selection, StateDef, StateTreeDef,
    StateTreeError, Target, TransitionDef, Trigger, UtilityDef, migrate_ron_v1_to_v2,
    state_tree_migration_sources, state_tree_schema_fingerprint,
};
pub(crate) use reload::watch_state_tree_sources;
pub use reload::{
    MAX_RELOAD_LOG, StateTreeReloadRecord, StateTreeReloads, StateTreeSource, read_state_tree_include,
};
pub use runtime::{
    MAX_PENDING_EVENTS, MAX_TRACE_ENTRIES, ReloadOutcome, StateTree, StateTreeRunStatus, TraceEvent,
    TraceKind,
};

use regia_app::prelude::*;
use regia_scene::layers::LayerParked;

use crate::task::BtContext;

/// Event names waiting to be delivered to the agent's [`StateTree`]. The tick system drains the
/// list into the tree each fixed step, in order. Reflected, so an agent can append an event with
/// `scene.set_property`.
///
/// While the tree is paused, or its agent is parked by a layer, nothing drains the list: it then
/// keeps at most [`MAX_PENDING_EVENTS`] names (the newest, in order). Each fixed step the oldest
/// past that cap are dropped and counted in `dropped`, and the first drop on an inbox logs one
/// warning (spec 38 § 5).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct StateTreeInbox {
    /// Event names, oldest first.
    pub events: Vec<String>,
    /// How many events were dropped, oldest first, because the inbox waited past
    /// [`MAX_PENDING_EVENTS`] while its tree was paused or its agent parked. Never reset.
    pub dropped: u64,
}

/// A reflected, read-only view of a state tree after its last tick: written every fixed step by
/// [`crate::AiPlugin`] next to the [`StateTree`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct StateTreeStatus {
    /// Where the tree is in its life.
    pub status: StateTreeRunStatus,
    /// Names of the active states, root first.
    pub active_path: Vec<String>,
    /// How many times the tree has ticked.
    pub ticks: u64,
    /// The recent trace, oldest first, one line per entry.
    pub trace: Vec<String>,
    /// Whether the tree is paused (`StateTree::pause`).
    pub paused: bool,
    /// Labels (`Owner -> Target`) of transitions whose `hold` counter is running.
    pub held: Vec<String>,
}

/// Dev-build debug controls for a [`StateTree`] (spec 38 § 5): states named in `disabled` are
/// skipped by selection and entry (as if their `enter` failed); entering a state named in
/// `pause_on_enter` pauses the tree at the end of that tick. Applied by the tick system; never
/// part of the asset. Compiled out of shipping builds (`cfg(debug_assertions)`).
#[cfg(debug_assertions)]
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct StateTreeDebug {
    /// State names to skip.
    pub disabled: Vec<String>,
    /// State names that pause the tree when entered.
    pub pause_on_enter: Vec<String>,
}

impl StateTreeStatus {
    /// Snapshots `tree`.
    pub fn of(tree: &StateTree) -> Self {
        Self {
            status: tree.status(),
            active_path: tree.active_path().into_iter().map(str::to_string).collect(),
            ticks: tree.ticks(),
            trace: tree.trace().map(ToString::to_string).collect(),
            paused: tree.is_paused(),
            held: tree.held(),
        }
    }

    /// Brings this status up to date with `tree`, touching only what changed: a steady tree
    /// allocates nothing here (spec 38 § 9), and trace lines are rebuilt only when one was added.
    fn refresh(&mut self, tree: &mut StateTree) {
        self.status = tree.status();
        self.ticks = tree.ticks();
        self.paused = tree.is_paused();
        if !tree
            .active_names()
            .eq(self.active_path.iter().map(String::as_str))
        {
            self.active_path = tree.active_names().map(str::to_string).collect();
        }
        if !tree.held_labels().eq(self.held.iter().map(String::as_str)) {
            self.held = tree.held();
        }
        if tree.take_trace_changed() {
            self.trace = tree.trace().map(ToString::to_string).collect();
        }
    }
}

/// `FixedUpdate` (exclusive): ticks every [`StateTree`] with take-run-reinsert semantics (the
/// same pattern as behaviour trees), delivers each agent's [`StateTreeInbox`] first, and writes a
/// [`StateTreeStatus`] snapshot afterwards.
pub(crate) fn run_state_trees(
    world: &mut World,
    query: &mut QueryState<Entity, (With<StateTree>, Without<LayerParked>)>,
    parked: &mut QueryState<(Entity, &mut StateTreeInbox), With<LayerParked>>,
    mut entities: Local<Vec<Entity>>,
) {
    tick_state_trees_with(world, query, &mut entities);
    cap_parked_inboxes(world, parked);
}

/// One pass of [`run_state_trees`] with its own queries and list, for a test that drives it by hand.
#[cfg(test)]
pub(crate) fn tick_state_trees(world: &mut World) {
    let mut query = world.query_filtered::<Entity, (With<StateTree>, Without<LayerParked>)>();
    tick_state_trees_with(world, &mut query, &mut Vec::new());
    let mut parked = world.query_filtered::<(Entity, &mut StateTreeInbox), With<LayerParked>>();
    cap_parked_inboxes(world, &mut parked);
}

/// Caps the inboxes of agents a layer has parked: the tick system skips them, so nothing else
/// drains their events (see [`cap_waiting_inbox`]).
fn cap_parked_inboxes(
    world: &mut World,
    parked: &mut QueryState<(Entity, &mut StateTreeInbox), With<LayerParked>>,
) {
    for (entity, mut inbox) in parked.iter_mut(world) {
        // Read through `Deref` first, so an inbox under the cap is not marked changed.
        if inbox.events.len() > MAX_PENDING_EVENTS {
            cap_waiting_inbox(entity, &mut inbox);
        }
    }
}

/// An inbox nothing is draining (its tree is paused, or its agent parked) keeps the newest
/// [`MAX_PENDING_EVENTS`] names: the oldest past that are dropped and counted in
/// `StateTreeInbox::dropped`, and the inbox's first drop logs a warning. So a host that sends an
/// event every step during a long pause neither grows the inbox without bound nor floods the
/// tree's queue on resume (the tree's queue holds the same number).
fn cap_waiting_inbox(entity: Entity, inbox: &mut StateTreeInbox) {
    let over = inbox.events.len().saturating_sub(MAX_PENDING_EVENTS);
    if over == 0 {
        return;
    }
    inbox.events.drain(..over);
    if inbox.dropped == 0 {
        tracing::warn!(
            target: "regia_ai",
            ?entity,
            cap = MAX_PENDING_EVENTS,
            "state tree inbox waiting while paused or parked is full; dropping its oldest events \
             (counted in StateTreeInbox.dropped, warned once per inbox)"
        );
    }
    inbox.dropped = inbox.dropped.saturating_add(over as u64);
}

#[tracing::instrument(skip_all, target = "regia_ai", name = "state_tree_tick")]
fn tick_state_trees_with(
    world: &mut World,
    query: &mut QueryState<Entity, (With<StateTree>, Without<LayerParked>)>,
    entities: &mut Vec<Entity>,
) {
    // The fixed step, never the frame delta, so replays do not depend on frame timing.
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    entities.clear();
    entities.extend(query.iter(world));
    crate::agent_order::<StateTree>(world, entities);
    // An `AiTickBudget`, when one is on, leaves in `entities` the trees of this step and gives each its
    // accumulated time (ADR-2709). Otherwise the list stays as it is and is empty.
    let budget_dts = crate::tick_budget::plan_state_trees(world, entities);
    // Trees compiled from a changed file wait here and are swapped in at the start of the agent's
    // tick, so a reload never lands in the middle of one.
    let mut pending = world
        .get_resource_mut::<StateTreeReloads>()
        .map(|mut reloads| reloads.take_pending())
        .unwrap_or_default();
    let mut records = Vec::new();
    for (index, &entity) in entities.iter().enumerate() {
        let Ok(mut agent) = world.get_entity_mut(entity) else {
            continue;
        };
        let Some(mut tree) = agent.take::<StateTree>() else {
            continue;
        };
        let dt = budget_dts.get(index).copied().unwrap_or(dt);
        let reload = pending
            .iter()
            .position(|(e, _, _)| *e == entity)
            .map(|i| pending.swap_remove(i));
        #[cfg(debug_assertions)]
        {
            if let Some(debug) = agent.get::<StateTreeDebug>() {
                tree.set_debug(&debug.disabled, &debug.pause_on_enter);
            }
        }
        // A paused tree takes nothing from its inbox: the events keep waiting there, in order,
        // and a burst sent while paused can neither fill the tree's queue nor write `dropped`
        // trace lines (spec 38 § 5: a paused tree advances nothing). The inbox keeps only the
        // newest `MAX_PENDING_EVENTS` meanwhile, so a long pause cannot grow it without bound.
        let inbox = if tree.is_paused() {
            let over = agent
                .get::<StateTreeInbox>()
                .is_some_and(|inbox| inbox.events.len() > MAX_PENDING_EVENTS);
            if over && let Some(mut inbox) = agent.get_mut::<StateTreeInbox>() {
                cap_waiting_inbox(entity, &mut inbox);
            }
            Vec::new()
        } else {
            agent
                .get_mut::<StateTreeInbox>()
                .map(|mut inbox| std::mem::take(&mut inbox.events))
                .unwrap_or_default()
        };
        for event in inbox {
            tree.send_event(&event);
        }
        let mut ctx = BtContext { world, entity, dt };
        if let Some((_, path, next)) = reload {
            let outcome = tree.reload(next, &mut ctx);
            records.push(StateTreeReloadRecord {
                entity,
                path,
                result: Ok(outcome),
            });
        }
        tree.tick(&mut ctx);
        // A task may have despawned its own agent, or installed a new tree on it. Either way the
        // old tree is dropped without `reset` (ADR-2396), so its spatial queries go with it. The
        // usual path reinserts this tree and keeps the queries its tasks still hold.
        let dropped = if let Ok(mut agent) = world.get_entity_mut(entity) {
            if agent.contains::<StateTree>() {
                true
            } else {
                match agent.get_mut::<StateTreeStatus>() {
                    Some(mut status) => status.refresh(&mut tree),
                    None => {
                        tree.take_trace_changed();
                        agent.insert(StateTreeStatus::of(&tree));
                    }
                }
                agent.insert(tree);
                false
            }
        } else {
            true
        };
        if dropped {
            crate::forget_spatial_queries(world, entity);
        }
    }
    crate::tick_budget::recycle_dts(world, budget_dts);
    // Trees for agents that did not tick (parked, gone, not scheduled this step) wait for a later step;
    // a despawned agent's tree is dropped.
    pending.retain(|(e, _, _)| world.get_entity(*e).is_ok());
    if !records.is_empty() || !pending.is_empty() {
        let mut reloads = world.get_resource_or_insert_with(StateTreeReloads::default);
        for record in records {
            reloads.push_record(record);
        }
        reloads.restore_pending(pending);
    }
}
