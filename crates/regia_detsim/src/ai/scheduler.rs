// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mod.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1389-1413 of `AiPipeline::run_world_inner`, plan row S-09). DO27's decision step (skip units
// not due, score the catalogue filtered by the repertoire and scaled by the option bias and clamped, commit
// through the decision loop into a `BTreeMap` by entity id) is kept; named changes: the unit type, the
// catalogue, the filter and the bias are the host's (`AiAgent`, the scoring closure, `score_options`), the
// decision loop is a `DecisionPolicy` (DO27's hysteresis is plan row S-11's), the units are visited in entity-id
// order whatever order the host passes them in, the scorer gets the unit's `AiRng::tick_stream`, and the step
// reports its work as counters.
// Rechecked at regia-absorb-4 (commit c9513623f): the only change to lines 1389-1429 since d6ed37677 is a profiling
// span around this step (DO27's `tick_prof` feature, off by default), which the `tracing` span in `decide` already
// is; the team-target build DO27 added after the step is the general `TeamRosters` in `roster.rs`.

//! [`AiCore`]: the time-sliced decision step.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::cadence::{ChannelQueue, DueScratch, due_into};
use super::utility::{AiOption, Inputs, OptionId, score};
use super::utility_catalogue::{InputValues, OptionIdx, UtilityCatalogue, score_idx};
use super::{AiCadence, AiRng, CadenceError, CadenceReport, ChannelId, QueuedUnit};
use crate::float::det_clamp;

/// A unit the AI core schedules: anything with a stable entity id.
pub trait AiAgent {
    /// The unit's entity id. It sets the unit's decision phase and its random stream, so it must be the same
    /// on every peer.
    fn entity_id(&self) -> u32;

    /// The unit's importance tier this tick ([`AiCadence::multiplier`] multiplies every channel's period for
    /// it). Default 0. It must come from sim state every peer shares (distance to the nearest enemy, a squad
    /// role), never from a camera or the wall clock.
    fn cadence_tier(&self) -> u8 {
        0
    }
}

impl AiAgent for u32 {
    fn entity_id(&self) -> u32 {
        *self
    }
}

/// How a unit's option scores become its committed decision.
///
/// [`ArgMax`] commits the best score each time. A policy that keeps a decision for a while (the hysteresis loop
/// of plan row S-11) keeps that state in its `Decision`.
///
/// `K` is the option key the scores carry: [`OptionId`] (DO27's closed set, the default) or [`OptionIdx`] (an
/// option of a [`UtilityCatalogue`]; [`ArgMaxIdx`]).
pub trait DecisionPolicy<K = OptionId> {
    /// The per-unit decision state, kept by [`AiCore`] per entity id.
    type Decision: Default;

    /// Commit a decision from this tick's scores (in the order the scorer produced them).
    fn commit(&self, decision: &mut Self::Decision, scores: &[(K, f32)]);

    /// The option a decision currently commits to.
    fn current(decision: &Self::Decision) -> Option<K>;
}

/// Commit the highest score; of equal best scores the last one wins (`max_by` with `total_cmp`), and no scores
/// commit [`OptionId::Hold`]. This is the choice DO27's hysteresis loop makes for a unit with no decision yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgMax;

impl DecisionPolicy for ArgMax {
    type Decision = Option<OptionId>;

    fn commit(&self, decision: &mut Option<OptionId>, scores: &[(OptionId, f32)]) {
        let best = scores
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| *id)
            .unwrap_or(OptionId::Hold);
        *decision = Some(best);
    }

    fn current(decision: &Option<OptionId>) -> Option<OptionId> {
        *decision
    }
}

/// [`ArgMax`] over a [`UtilityCatalogue`]'s options: commit the highest score, the last of equal best scores
/// (`max_by` with `total_cmp`, the same rule as [`ArgMax`]); no scores commit nothing (`None`), since a game's
/// catalogue has no fixed fallback option.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgMaxIdx;

impl DecisionPolicy<OptionIdx> for ArgMaxIdx {
    type Decision = Option<OptionIdx>;

    fn commit(&self, decision: &mut Option<OptionIdx>, scores: &[(OptionIdx, f32)]) {
        *decision = scores
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| *id);
    }

    fn current(decision: &Option<OptionIdx>) -> Option<OptionIdx> {
        *decision
    }
}

/// Score a catalogue for one unit: each option `allowed` passes is scored ([`score`]), scaled by `bias` and
/// clamped to 0..1, and pushed to `out` in catalogue order.
///
/// `allowed` is where a repertoire mask or a team ability filter plugs in, `bias` where a difficulty's option
/// bias does; `|_| true` and `|_| 1.0` score the catalogue as it is.
pub fn score_options(
    catalogue: &[AiOption],
    inputs: &Inputs,
    mut allowed: impl FnMut(OptionId) -> bool,
    mut bias: impl FnMut(OptionId) -> f32,
    out: &mut Vec<(OptionId, f32)>,
) {
    for option in catalogue {
        if !allowed(option.id) {
            continue;
        }
        let s = score(option, inputs) * bias(option.id);
        out.push((option.id, det_clamp(s, 0.0, 1.0)));
    }
}

/// [`score_options`] over a [`UtilityCatalogue`]: each option (in index order) that `allowed` passes is scored
/// ([`score_idx`] on `inputs`), scaled by `bias` and clamped to 0..1, and pushed to `out`. For the DO27 preset
/// ([`UtilityCatalogue::do27_preset`]) with `inputs` = [`Inputs::values`] and the same filter and bias, the
/// scores are [`score_options`]'s, bit for bit, with [`OptionId::idx`] keys.
pub fn score_options_idx(
    catalogue: &UtilityCatalogue,
    inputs: &InputValues,
    mut allowed: impl FnMut(OptionIdx) -> bool,
    mut bias: impl FnMut(OptionIdx) -> f32,
    out: &mut Vec<(OptionIdx, f32)>,
) {
    for (i, option) in catalogue.options().iter().enumerate() {
        // The catalogue cap (64 options) keeps the index in range.
        let idx = OptionIdx(i as u16);
        if !allowed(idx) {
            continue;
        }
        let s = score_idx(option, inputs) * bias(idx);
        out.push((idx, det_clamp(s, 0.0, 1.0)));
    }
}

/// What one [`AiCore::decide`] call did, as counters (no timing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiTickReport {
    /// The tick.
    pub tick: u64,
    /// Units that were due and re-scored.
    pub due: u32,
    /// Option scores the scorer produced for them.
    pub scored_options: u32,
}

/// One unit's decision state for a debug view or an agent method. `K` is the option key of the core's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiDecisionRow<K = OptionId> {
    /// The entity id.
    pub entity: u32,
    /// The committed option, if any.
    pub current: Option<K>,
    /// The tick (mod the decision cadence) on which the unit re-scores.
    pub decision_phase: u32,
    /// The next tick, at or after the one asked about, on which the unit re-scores.
    pub next_decision_tick: u64,
}

/// The deterministic AI decision step: each tick, the units whose decision phase is due re-score their
/// options and commit through the [`DecisionPolicy`].
///
/// - **Time slicing.** A unit is due when `tick % cadence == entity_id % cadence` ([`AiCadence`]), so 60
///   units on the default 10-tick cadence re-score 6 per tick, and each unit exactly once per window. The
///   `decide` channel's phase rule, tiers ([`AiAgent::cadence_tier`]) and cap apply; other channels' due lists
///   come from [`Self::due_units`].
/// - **Order.** Due units are visited in ascending entity id (ties keep the order of the slice), whatever
///   order the host passes them in, so a scorer with side effects still runs the same on every peer.
/// - **Randomness.** The scorer gets the unit's [`AiRng::tick_stream`] for this tick; no other source.
/// - **State.** Decisions live in a `BTreeMap` by entity id. A unit missing from a tick keeps its decision;
///   call [`Self::forget`] or [`Self::retain`] when units leave.
#[derive(Debug, Clone, Serialize, Deserialize)]
///
/// `K` is the option key ([`DecisionPolicy`]): [`OptionId`] by default; `AiCore<ArgMaxIdx, OptionIdx>` decides
/// over a [`UtilityCatalogue`] ([`score_options_idx`]).
#[serde(bound(
    serialize = "P: Serialize, P::Decision: Serialize",
    deserialize = "P: Deserialize<'de>, P::Decision: Deserialize<'de>"
))]
pub struct AiCore<P = ArgMax, K = OptionId>
where
    P: DecisionPolicy<K>,
{
    world_seed: u64,
    cadence: AiCadence,
    policy: P,
    decisions: BTreeMap<u32, P::Decision>,
    /// Each channel's carry-over queue (by channel index; sim state, kept in snapshots).
    #[serde(default)]
    queues: Vec<ChannelQueue>,
    /// Scratch for the due lists.
    #[serde(skip)]
    due_scratch: DueScratch,
    /// Scratch: the slice indices of the due units, sorted by entity id.
    #[serde(skip)]
    order: Vec<usize>,
    /// Scratch: one unit's scores.
    #[serde(skip)]
    scores: Vec<(K, f32)>,
}

impl<P: DecisionPolicy<K>, K> AiCore<P, K> {
    /// An AI core with no decisions yet.
    pub fn new(world_seed: u64, cadence: AiCadence, policy: P) -> Self {
        AiCore {
            world_seed,
            cadence,
            policy,
            decisions: BTreeMap::new(),
            queues: Vec::new(),
            due_scratch: DueScratch::default(),
            order: Vec::new(),
            scores: Vec::new(),
        }
    }

    /// The world seed the random streams mix in.
    pub fn world_seed(&self) -> u64 {
        self.world_seed
    }

    /// The schedule.
    pub fn cadence(&self) -> &AiCadence {
        &self.cadence
    }

    /// The decision policy.
    pub fn policy(&self) -> &P {
        &self.policy
    }

    /// The decision policy, to change its settings.
    pub fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    /// Run one tick's slice: for each unit of `units` that is due on `tick`, in entity-id order, call
    /// `score_unit(unit, rng, scores)` with the unit's tick stream and an empty score list, then commit the
    /// scores through the policy.
    pub fn decide<U, F>(&mut self, tick: u64, units: &[U], mut score_unit: F) -> AiTickReport
    where
        U: AiAgent,
        F: FnMut(&U, &mut AiRng, &mut Vec<(K, f32)>),
    {
        let _span = tracing::trace_span!("regia_detsim::ai::decide", tick, units = units.len()).entered();
        let AiCore {
            world_seed,
            cadence,
            policy,
            decisions,
            queues,
            due_scratch,
            order,
            scores,
        } = self;
        let queue = queue_for(queues, ChannelId::DECIDE);
        // The decide channel always exists, so this cannot fail; an error would leave `order` empty.
        let _ = due_into(cadence, ChannelId::DECIDE, queue, due_scratch, tick, units, order);
        let mut report = AiTickReport {
            tick,
            due: 0,
            scored_options: 0,
        };
        for &i in order.iter() {
            let unit = &units[i];
            let id = unit.entity_id();
            scores.clear();
            let mut rng = AiRng::tick_stream(*world_seed, tick, id);
            score_unit(unit, &mut rng, scores);
            policy.commit(decisions.entry(id).or_default(), scores);
            report.due = report.due.saturating_add(1);
            report.scored_options = report.scored_options.saturating_add(scores.len() as u32);
        }
        report
    }

    /// Every unit's decision state, by entity id.
    pub fn decisions(&self) -> &BTreeMap<u32, P::Decision> {
        &self.decisions
    }

    /// One unit's decision state.
    pub fn decision(&self, entity_id: u32) -> Option<&P::Decision> {
        self.decisions.get(&entity_id)
    }

    /// The option a unit is committed to.
    pub fn decision_for(&self, entity_id: u32) -> Option<K> {
        self.decisions.get(&entity_id).and_then(P::current)
    }

    /// A unit's decision state to change outside the schedule, created with `Default` if the unit has none:
    /// an order that commits an option at once (a squad command) writes it here, whether or not the unit is
    /// due this tick.
    pub fn decision_entry(&mut self, entity_id: u32) -> &mut P::Decision {
        self.decisions.entry(entity_id).or_default()
    }

    /// Drop a unit's decision and its place in every channel queue (it left the match or died).
    pub fn forget(&mut self, entity_id: u32) {
        self.decisions.remove(&entity_id);
        for queue in &mut self.queues {
            queue.pending.retain(|q| q.entity != entity_id);
        }
    }

    /// Keep only the decisions (and channel queue places) of the units `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) {
        self.decisions.retain(|id, _| keep(*id));
        for queue in &mut self.queues {
            queue.pending.retain(|q| keep(q.entity));
        }
    }

    /// The units that run on `channel` this tick, as indices into `units`, in ascending entity id (equal ids
    /// keep slice order), written to `out` (cleared first). Any stage (perception, target picks, routing,
    /// squad orders) gets its slice here.
    ///
    /// A unit is due on its phase for its tier ([`AiAgent::cadence_tier`]). Under the channel's cap the oldest
    /// waiting units run first (wait descending, then entity id ascending) and the rest wait in the channel's
    /// queue, which is sim state (kept in snapshots). A waiting unit that falls due again keeps its older
    /// place. Pass every unit of the channel each tick: a unit missing from a call loses its place in the
    /// queue. Call it once per channel per tick: [`Self::decide`] already takes the `decide` channel's list,
    /// so a second call on `decide` for the same tick would advance its queue twice. No allocation once `out`
    /// and the core's buffers are warm.
    pub fn due_units<U: AiAgent>(
        &mut self,
        channel: ChannelId,
        tick: u64,
        units: &[U],
        out: &mut Vec<usize>,
    ) -> Result<CadenceReport, CadenceError> {
        let _span = tracing::trace_span!(
            "regia_detsim::ai::due_units",
            tick,
            channel = channel.index(),
            units = units.len()
        )
        .entered();
        if self.cadence.channel(channel).is_none() {
            out.clear();
            return Err(CadenceError::UnknownChannel {
                index: channel.index() as u16,
            });
        }
        let queue = queue_for(&mut self.queues, channel);
        due_into(
            &self.cadence,
            channel,
            queue,
            &mut self.due_scratch,
            tick,
            units,
            out,
        )
    }

    /// The counters of the last [`Self::due_units`] (or [`Self::decide`] for `decide`) call on `channel`;
    /// `None` before the first call or after a snapshot was read.
    pub fn cadence_report(&self, channel: ChannelId) -> Option<CadenceReport> {
        self.queues.get(channel.index()).and_then(|q| q.last)
    }

    /// The units waiting in `channel`'s queue, ascending entity id. `None`: unknown channel.
    pub fn queued(&self, channel: ChannelId) -> Option<&[QueuedUnit]> {
        self.cadence.channel(channel)?;
        Some(
            self.queues
                .get(channel.index())
                .map_or(&[], |q| q.pending.as_slice()),
        )
    }

    /// One row per unit with a decision, in entity-id order. `next_tick` is the next tick the host will run;
    /// each row's `next_decision_tick` is at or after it.
    pub fn rows(&self, next_tick: u64) -> Vec<AiDecisionRow<K>> {
        self.decisions
            .iter()
            .map(|(&entity, decision)| AiDecisionRow {
                entity,
                current: P::current(decision),
                decision_phase: self.cadence.decision_phase(entity),
                next_decision_tick: self.cadence.next_decision_tick(next_tick, entity),
            })
            .collect()
    }
}

/// The queue of `channel`, growing the list to reach it.
fn queue_for(queues: &mut Vec<ChannelQueue>, channel: ChannelId) -> &mut ChannelQueue {
    let index = channel.index();
    if queues.len() <= index {
        queues.resize_with(index + 1, ChannelQueue::default);
    }
    &mut queues[index]
}
