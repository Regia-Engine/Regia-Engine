// SPDX-License-Identifier: MPL-2.0
//! Bounded prediction and rollback for a game-owned step (spec 120 § 9.5, ADR-7953).
//!
//! The game owns the state `S` and the step function. This module keeps a fixed number of
//! per-tick snapshots and the inputs that produced them, and replays the game's step from a
//! corrected tick. It does not use the ECS or the app. The ECS predictor
//! (`NetVisualOffset` in `predict`) is a separate path.
//!
//! Memory is at most `capacity_ticks` copies of `S` plus the [`TickInputs`] stored with each
//! tick. A tick that has fallen off the front is gone: [`RollbackLog::reconcile`] returns
//! [`RollbackError::RollbackTooOld`] and does not change the stored snapshots or inputs.

use crate::tick::TickInputs;

/// Counters a host publishes for `net.rollback` (spec 120 § 13.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollbackCounters {
    /// Successful reconciles that re-simulated at least one tick.
    pub replays: u64,
    /// Largest `newest_tick - server_tick` of a successful reconcile.
    pub max_depth: u64,
    /// Times `reconcile` or `amend_inputs` returned `rollback_too_old`.
    pub too_old: u64,
}

/// Why a rollback read or write was refused.
///
/// [`RollbackError::code`] is the stable lowercase token (`rollback_too_old`,
/// `rollback_not_recorded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RollbackError {
    /// `tick` is older than every snapshot the ring still holds.
    #[error("rollback_too_old: tick {tick} is older than the ring (oldest {oldest:?})")]
    RollbackTooOld {
        /// The tick that was refused.
        tick: u64,
        /// The oldest tick still stored, or `None` when the ring is empty.
        oldest: Option<u64>,
    },
    /// `tick` is not stored, and it is not older than the ring (a gap, or ahead of the newest record).
    #[error("rollback_not_recorded: tick {tick} is not in the ring")]
    NotRecorded {
        /// The tick that is not stored.
        tick: u64,
    },
}

impl RollbackError {
    /// The stable error code: `rollback_too_old` or `rollback_not_recorded`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::RollbackTooOld { .. } => "rollback_too_old",
            Self::NotRecorded { .. } => "rollback_not_recorded",
        }
    }
}

#[derive(Clone, Debug)]
struct Slot<S, I> {
    tick: u64,
    state: S,
    inputs: TickInputs<I>,
}

/// Bounded ring of game-owned snapshots and inputs for prediction and rollback without the ECS
/// (spec 120 § 9.5).
#[derive(Clone, Debug)]
pub struct RollbackLog<S, I> {
    capacity: u32,
    slots: Vec<Slot<S, I>>,
    counters: RollbackCounters,
}

impl<S: Clone, I: Clone> RollbackLog<S, I> {
    /// A log that keeps at most `capacity_ticks` ticks.
    ///
    /// The retained memory is `capacity_ticks` snapshots of `S` plus the inputs stored beside
    /// each (spec 120 § 9.5). `capacity_ticks == 0` keeps nothing: every reconcile and amend
    /// returns [`RollbackError::RollbackTooOld`].
    #[must_use]
    pub fn new(capacity_ticks: u32) -> Self {
        let cap = usize::try_from(capacity_ticks).unwrap_or(usize::MAX);
        Self {
            capacity: capacity_ticks,
            slots: Vec::with_capacity(cap.min(64)),
            counters: RollbackCounters::default(),
        }
    }

    /// How many ticks this log retains at most.
    #[must_use]
    pub fn capacity_ticks(&self) -> u32 {
        self.capacity
    }

    /// Snapshots currently stored, never more than [`Self::capacity_ticks`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True when no tick is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Oldest stored tick.
    #[must_use]
    pub fn oldest_tick(&self) -> Option<u64> {
        self.slots.first().map(|slot| slot.tick)
    }

    /// Newest stored tick.
    #[must_use]
    pub fn newest_tick(&self) -> Option<u64> {
        self.slots.last().map(|slot| slot.tick)
    }

    /// Stored ticks, oldest first.
    pub fn ticks(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots.iter().map(|slot| slot.tick)
    }

    /// The state recorded for `tick`, if it is still in the ring.
    #[must_use]
    pub fn state(&self, tick: u64) -> Option<&S> {
        self.position(tick).map(|index| &self.slots[index].state)
    }

    /// The inputs recorded for `tick`, if it is still in the ring.
    #[must_use]
    pub fn inputs(&self, tick: u64) -> Option<&TickInputs<I>> {
        self.position(tick).map(|index| &self.slots[index].inputs)
    }

    /// Replays, the deepest successful replay, and too-old refusals.
    #[must_use]
    pub fn counters(&self) -> RollbackCounters {
        self.counters
    }

    /// Records the state after stepping `tick` and the inputs used for it.
    ///
    /// Recording a tick that is already stored replaces that slot. Recording a new tick drops
    /// the oldest snapshots until the ring is within [`Self::capacity_ticks`].
    pub fn record(&mut self, tick: u64, state: S, inputs: TickInputs<I>) {
        if self.capacity == 0 {
            return;
        }
        if let Some(index) = self.position(tick) {
            self.slots[index].state = state;
            self.slots[index].inputs = inputs;
            return;
        }
        let index = self.slots.partition_point(|slot| slot.tick < tick);
        self.slots.insert(index, Slot { tick, state, inputs });
        let cap = usize::try_from(self.capacity).unwrap_or(usize::MAX);
        while self.slots.len() > cap {
            self.slots.remove(0);
        }
    }

    /// Replaces the inputs of a past tick (a late real input arrived).
    ///
    /// The stored state is left as it was. The next [`Self::reconcile`] replays with the new inputs.
    /// A tick older than the ring returns [`RollbackError::RollbackTooOld`] and changes nothing
    /// except the too-old counter.
    pub fn amend_inputs(&mut self, tick: u64, inputs: TickInputs<I>) -> Result<(), RollbackError> {
        let index = self.locate(tick)?;
        self.slots[index].inputs = inputs;
        Ok(())
    }

    /// Restores `server_state` at `server_tick` and replays the stored inputs through `step`
    /// up to the newest tick.
    ///
    /// `step` is called once for each tick in `(server_tick, newest]`, which is
    /// `newest - server_tick` calls when that range was recorded without a gap. The tick
    /// argument is the tick being produced (the state after the call is stored at that tick).
    /// Returns the corrected newest state.
    ///
    /// [`RollbackError::RollbackTooOld`] (`rollback_too_old`) means `server_tick` is older than
    /// the ring. The stored snapshots and inputs are unchanged. A tick that is not older than
    /// the ring but was never stored, or a gap between `server_tick` and the newest tick,
    /// returns [`RollbackError::NotRecorded`] and also changes nothing.
    pub fn reconcile(
        &mut self,
        server_tick: u64,
        server_state: S,
        mut step: impl FnMut(&mut S, u64, &TickInputs<I>),
    ) -> Result<&S, RollbackError> {
        let server_index = self.locate(server_tick)?;
        self.suffix_is_contiguous(server_index)?;
        let newest = self.slots[self.slots.len() - 1].tick;
        self.slots[server_index].state = server_state;
        let mut index = server_index;
        while self.slots[index].tick != newest {
            let next = index + 1;
            let tick = self.slots[next].tick;
            let mut state = self.slots[index].state.clone();
            step(&mut state, tick, &self.slots[next].inputs);
            self.slots[next].state = state;
            index = next;
        }
        let depth = newest - server_tick;
        if depth > 0 {
            self.counters.replays = self.counters.replays.saturating_add(1);
            if depth > self.counters.max_depth {
                self.counters.max_depth = depth;
            }
        }
        Ok(&self.slots[index].state)
    }

    fn position(&self, tick: u64) -> Option<usize> {
        self.slots.iter().position(|slot| slot.tick == tick)
    }

    fn locate(&mut self, tick: u64) -> Result<usize, RollbackError> {
        if let Some(index) = self.position(tick) {
            return Ok(index);
        }
        let too_old = match self.slots.first() {
            None => true,
            Some(oldest) => tick < oldest.tick,
        };
        if too_old {
            let oldest = self.oldest_tick();
            self.counters.too_old = self.counters.too_old.saturating_add(1);
            Err(RollbackError::RollbackTooOld { tick, oldest })
        } else {
            Err(RollbackError::NotRecorded { tick })
        }
    }

    /// Every tick from `server_index` through the newest differs by exactly one.
    fn suffix_is_contiguous(&self, server_index: usize) -> Result<(), RollbackError> {
        let mut expect = self.slots[server_index].tick;
        for slot in &self.slots[server_index..] {
            if slot.tick != expect {
                return Err(RollbackError::NotRecorded { tick: expect });
            }
            expect = expect.saturating_add(1);
        }
        Ok(())
    }
}

/// A presentation offset that decays to zero over a fixed number of ticks (no wall clock).
///
/// The host adds [`VisualOffset::value`] to the simulated coordinate. After `span_ticks` calls
/// to [`VisualOffset::advance`] the value is zero, and its magnitude never increases. This is
/// the game-owned twin of the ECS `NetVisualOffset`: that one decays by a half-life in
/// milliseconds; this one counts ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualOffset {
    span_ticks: u32,
    remaining: u32,
    start: i64,
}

impl VisualOffset {
    /// An offset of `delta` now that is zero after `span_ticks` advances.
    ///
    /// A span of zero, or a delta of zero, is already zero.
    #[must_use]
    pub fn new(delta: i64, span_ticks: u32) -> Self {
        if span_ticks == 0 || delta == 0 {
            Self {
                span_ticks: 0,
                remaining: 0,
                start: 0,
            }
        } else {
            Self {
                span_ticks,
                remaining: span_ticks,
                start: delta,
            }
        }
    }

    /// The offset that hides a snap from `predicted` to `corrected`.
    ///
    /// [`Self::apply`] of `corrected` starts at `predicted` and reaches `corrected` after
    /// `span_ticks` advances. The subtraction saturates.
    #[must_use]
    pub fn from_correction(predicted: i64, corrected: i64, span_ticks: u32) -> Self {
        Self::new(predicted.saturating_sub(corrected), span_ticks)
    }

    /// The offset to add to the simulated value. Zero once the span has elapsed.
    ///
    /// The value is `start * remaining / span`, with division toward zero, so the magnitude
    /// falls or stays as `remaining` falls, and it is exactly zero at the end.
    #[must_use]
    pub fn value(self) -> i64 {
        if self.span_ticks == 0 || self.remaining == 0 {
            return 0;
        }
        let scaled = i128::from(self.start) * i128::from(self.remaining) / i128::from(self.span_ticks);
        i64::try_from(scaled).unwrap_or(self.start)
    }

    /// One tick of decay. Does not read a clock.
    pub fn advance(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
    }

    /// `simulated + value`, saturating.
    #[must_use]
    pub fn apply(self, simulated: i64) -> i64 {
        simulated.saturating_add(self.value())
    }

    /// The span passed to [`Self::new`], or zero when the offset started at zero.
    #[must_use]
    pub fn span_ticks(self) -> u32 {
        self.span_ticks
    }

    /// True when [`Self::value`] is zero.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.value() == 0
    }
}
