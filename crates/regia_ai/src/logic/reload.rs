// SPDX-License-Identifier: MPL-2.0
//! Queued logic-graph hot reloads (spec 102 § 7): a host compiles new text with
//! `LogicGraph::compile` and queues the compiled graph here; the tick system swaps it in at the start
//! of that entity's next tick (never in the middle of one) and logs the outcome. A definition that
//! fails to compile is recorded with `LogicReloads::reject` and never reaches the graph.

use std::collections::VecDeque;

use regia_app::prelude::*;

use super::runtime::{LogicGraph, LogicReloadOutcome};

/// The most records [`LogicReloads`] keeps; the oldest are dropped first.
pub const MAX_LOGIC_RELOAD_LOG: usize = 32;

/// What happened to one reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicReloadRecord {
    /// The graph entity.
    pub entity: Entity,
    /// Where the definition came from (a project path, or a label such as `agent`).
    pub source: String,
    /// The swap outcome, or why the definition was rejected (the graph kept running unchanged).
    pub result: Result<LogicReloadOutcome, String>,
}

/// Compiled graphs waiting for their entity's next tick, plus the log of what happened.
#[derive(Resource, Default)]
pub struct LogicReloads {
    pending: Vec<(Entity, String, LogicGraph)>,
    log: VecDeque<LogicReloadRecord>,
    /// Swaps applied so far.
    pub applied: u64,
    /// Definitions rejected so far.
    pub rejected: u64,
}

impl LogicReloads {
    /// Queues `graph` for `entity`; it is swapped in at the start of the entity's next tick. A newer
    /// graph for the same entity replaces an older one that has not been applied yet.
    pub fn queue(&mut self, entity: Entity, source: impl Into<String>, graph: LogicGraph) {
        self.pending.retain(|(e, _, _)| *e != entity);
        self.pending.push((entity, source.into(), graph));
    }

    /// Records a definition that did not compile: the graph keeps running unchanged.
    pub fn reject(&mut self, entity: Entity, source: impl Into<String>, reason: impl Into<String>) {
        self.rejected += 1;
        self.push_record(LogicReloadRecord {
            entity,
            source: source.into(),
            result: Err(reason.into()),
        });
    }

    /// How many graphs wait for their entity's next tick.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The most recent records, oldest first.
    pub fn log(&self) -> impl Iterator<Item = &LogicReloadRecord> {
        self.log.iter()
    }

    /// Appends a record, dropping the oldest past [`MAX_LOGIC_RELOAD_LOG`].
    pub fn push_record(&mut self, record: LogicReloadRecord) {
        if record.result.is_ok() {
            self.applied += 1;
        }
        if self.log.len() >= MAX_LOGIC_RELOAD_LOG {
            self.log.pop_front();
        }
        self.log.push_back(record);
    }

    /// Takes the queued graph of `entity`, if any.
    pub(crate) fn take_for(&mut self, entity: Entity) -> Option<(String, LogicGraph)> {
        let index = self.pending.iter().position(|(e, _, _)| *e == entity)?;
        let (_, source, graph) = self.pending.swap_remove(index);
        Some((source, graph))
    }

    /// Drops queued graphs whose entity is gone.
    pub(crate) fn retain_live(&mut self, world: &World) {
        self.pending.retain(|(e, _, _)| world.get_entity(*e).is_ok());
    }
}
