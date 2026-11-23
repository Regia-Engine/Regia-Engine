// SPDX-License-Identifier: MPL-2.0
//! Rollback lockstep for small action matches (spec 120 § 7.7, ADR-8361).
//!
//! A [`RollbackRunner`] drives a [`LockstepSeat`] under [`LockstepMode::Rollback`]: instead of
//! waiting for each frame, the seat steps its simulation on its own clock with predicted inputs,
//! at most the rollback window past the newest frame it has, and replays from the first tick
//! whose real inputs differ from the prediction when the frame arrives (the model of the public
//! GGPO documentation). The frames are the strict match's (the host holds a frame for a live
//! seat's input for up to twice the window), so the confirmed state is the strict lockstep state, bit
//! for bit, and only confirmed ticks are hashed and voted on.
//!
//! The prediction of a tick: the seat's own input as it sampled it, every other seat's last
//! input from the newest frame with its presses cleared ([`InputRules::clear_presses`]; without
//! rules the last input as it was), every seat absent on a gap tick of a delay raise. The game's
//! state and step stay the game's ([`RollbackGame`]); the snapshots live in a `RollbackLog`
//! (spec 120 § 9.5) of the window plus two ticks.
//!
//! [`LockstepMode::Rollback`]: super::LockstepMode::Rollback
//! [`InputRules::clear_presses`]: super::InputRules::clear_presses

use std::collections::BTreeMap;
use std::ops::Range;

use super::{LockstepConfig, LockstepFrame, LockstepSeat, MembershipChange, SeatId, SeatInput, Stamp};
use crate::feedback::{FEEDBACK_WINDOW_MS, LEAD_CALM_MS};
use crate::payload::{NetCodec, NetGame};
use crate::rollback::RollbackLog;
use crate::session::PeerId;
use crate::tick::TickInputs;

/// The game's deterministic simulation as a rollback runner steps it. The same step a strict
/// seat runs on each frame; a predicted tick is stepped from a frame the runner builds.
pub trait RollbackGame<I>: Clone {
    /// Steps one tick with `frame`'s inputs.
    fn step(&mut self, frame: &LockstepFrame<I>);
    /// The state hash after a step: reported for confirmed ticks only.
    fn hash(&self) -> u64;
}

/// What one [`RollbackRunner::advance`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RollbackAdvance {
    /// The ticks frames confirmed in this call.
    pub confirmed: Range<u64>,
    /// Ticks replayed by the deepest rollback of this call (`None`: every prediction held).
    pub rolled_back: Option<u64>,
    /// Ticks stepped ahead of the frames with predicted inputs.
    pub predicted: u32,
    /// The seat's clock was past the window: it waits for a frame.
    pub stalled: bool,
    /// Local inputs the seat refused to stamp (the match ended, or an input too large).
    pub inputs_refused: u32,
}

/// The fields of a confirmed frame beyond its inputs, kept while a replay may step it again.
#[derive(Debug, Clone)]
struct Meta {
    membership: Vec<MembershipChange>,
    match_end: bool,
    delay: u16,
    delay_from: u64,
}

/// A median input margin above this many ticks for five calm seconds moves a rollback seat's
/// clock back by one tick (the seat runs further ahead than its inputs need).
const SYNC_MEDIAN_HIGH_TICKS: i64 = 2;

fn peer(seat: SeatId) -> PeerId {
    PeerId(u32::from(seat.0))
}

fn seat_of(peer: PeerId) -> SeatId {
    SeatId(u16::try_from(peer.0).unwrap_or(u16::MAX))
}

/// Prediction, run-ahead and rollback for one seat of a rollback lockstep match (spec 120 § 7.7).
///
/// Call [`Self::advance`] once per application frame, after the seat took what the host sent
/// and before [`LockstepSeat::outgoing`]: it steps the frames that arrived (replaying the ticks
/// after the first misprediction), reports their hashes, then steps ahead on predictions up to
/// the tick the seat's clock has reached, sampling one local input for each tick it steps.
/// [`Self::state`] is what to present; [`Self::confirmed_state`] is the match's state.
pub struct RollbackRunner<G: NetGame, S> {
    seat: SeatId,
    log: RollbackLog<S, SeatInput<G::Input>>,
    window: u64,
    tick_us: u64,
    /// The next tick to step, predicted or confirmed. The log's tick `t` holds the state before
    /// game tick `t` (the state after `t - 1`).
    next_tick: u64,
    /// The first tick no frame has confirmed yet.
    confirmed_next: u64,
    /// The seats of the newest frame, with the last input each sent or was held with.
    last: BTreeMap<SeatId, Option<G::Input>>,
    /// `(delay, delay_from)` of the newest frame.
    delay: (u16, u64),
    /// Confirmed frames' other fields, while a replay of this call may step them.
    meta: BTreeMap<u64, Meta>,
    /// This seat's own inputs by the tick they are stamped for, with their capture tick.
    own: BTreeMap<u64, (u64, G::Input)>,
    /// The seat's clock: microseconds at which tick 0 was due here.
    anchor_us: Option<u64>,
    /// Ticks the clock runs ahead of the anchor (negative: behind), steered from the host's
    /// input margins so the seat's inputs reach the host before their frames are due.
    clock_lead: i64,
    /// When the clock lead last changed: the margins of one feedback window after it still
    /// describe the old clock.
    changed_at_ms: Option<u64>,
    /// The host tick of the last feedback record acted on.
    feedback_tick: Option<u64>,
    calm_since_ms: Option<u64>,
}

impl<G: NetGame, S> std::fmt::Debug for RollbackRunner<G, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollbackRunner")
            .field("seat", &self.seat)
            .field("next_tick", &self.next_tick)
            .field("confirmed_next", &self.confirmed_next)
            .finish_non_exhaustive()
    }
}

impl<G: NetGame, S: RollbackGame<G::Input>> RollbackRunner<G, S> {
    /// A runner for `seat` that starts from `initial`, the state before tick 0. `config` is the
    /// match's (its [`LockstepMode`](super::LockstepMode) gives the window; in strict mode the
    /// window is one tick, so the runner predicts only the tick whose frame is next).
    pub fn new(config: &LockstepConfig, seat: SeatId, initial: S) -> Self {
        let config = config.clone().sanitized();
        let window = u64::from(config.mode.window_ticks().max(1));
        let mut log = RollbackLog::new(u32::try_from(window + 2).unwrap_or(u32::MAX));
        log.record(0, initial, TickInputs::new());
        Self {
            seat,
            log,
            window,
            tick_us: config.tick_us(),
            next_tick: 0,
            confirmed_next: 0,
            last: BTreeMap::new(),
            delay: (config.initial_delay_ticks, 0),
            meta: BTreeMap::new(),
            own: BTreeMap::new(),
            anchor_us: None,
            clock_lead: 0,
            changed_at_ms: None,
            feedback_tick: None,
            calm_since_ms: None,
        }
    }

    /// The presented state: after the newest tick stepped, predicted or confirmed.
    pub fn state(&self) -> &S {
        self.log
            .state(self.next_tick)
            .or_else(|| self.log.newest_tick().and_then(|t| self.log.state(t)))
            .unwrap_or_else(|| unreachable_state())
    }

    /// The confirmed state: after the newest tick a frame confirmed (the initial state before
    /// any).
    pub fn confirmed_state(&self) -> &S {
        self.log
            .state(self.confirmed_next)
            .or_else(|| self.log.oldest_tick().and_then(|t| self.log.state(t)))
            .unwrap_or_else(|| unreachable_state())
    }

    /// The next tick to step: the presented state is after `tick() - 1`.
    pub fn tick(&self) -> u64 {
        self.next_tick
    }

    /// The first tick no frame has confirmed: the confirmed state is after `confirmed_tick() - 1`.
    pub fn confirmed_tick(&self) -> u64 {
        self.confirmed_next
    }

    /// Ticks stepped ahead of the newest frame.
    pub fn ahead_ticks(&self) -> u64 {
        self.next_tick - self.confirmed_next
    }

    /// The rollback window, ticks.
    pub fn window_ticks(&self) -> u64 {
        self.window
    }

    /// Ticks the seat's clock runs ahead of the first frame's arrival (negative: behind): the
    /// clock sync's correction, at most the window either way.
    pub fn clock_lead_ticks(&self) -> i64 {
        self.clock_lead
    }

    /// The clock sync (spec 120 § 7.7, ADR-8361): the host's input timing record says how far
    /// before their frames were due this seat's inputs arrived (margins in ticks against the
    /// host's frame clock). The clock is steered so the median input arrives on time: a median
    /// margin below zero moves the clock ahead at once by the deficit, one above
    /// `SYNC_MEDIAN_HIGH_TICKS` for five calm seconds moves it back by one tick. After a change the
    /// records of one feedback window, still measured under the old clock, are not acted on. The
    /// late tail is the host's hold's job. Every seat aligned to the host's frame clock is every
    /// seat equally ahead of the others: the frame advantage stays balanced.
    fn sync_clock(&mut self, seat: &LockstepSeat<G>, now_ms: u64) {
        let Some(fb) = seat.link_feedback().copied() else {
            return;
        };
        if fb.margin_samples == 0 || self.feedback_tick == Some(fb.server_tick) {
            return;
        }
        self.feedback_tick = Some(fb.server_tick);
        if self
            .changed_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < FEEDBACK_WINDOW_MS)
        {
            return;
        }
        let median = i64::from(fb.margin_p50);
        let bound = i64::try_from(self.window).unwrap_or(i64::MAX);
        let before = self.clock_lead;
        if median < 0 {
            self.clock_lead = (self.clock_lead - median).min(bound);
            self.calm_since_ms = None;
        } else if median > SYNC_MEDIAN_HIGH_TICKS {
            let since = *self.calm_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= LEAD_CALM_MS {
                self.clock_lead = (self.clock_lead - 1).max(-bound);
                self.calm_since_ms = None;
            }
        } else {
            self.calm_since_ms = None;
        }
        if self.clock_lead != before {
            self.changed_at_ms = Some(now_ms);
        }
    }

    /// Takes `state` as the state before tick `next_tick`, confirmed: after a state transfer or a
    /// catch-up (spec 120 § 7.4). Predictions are dropped; the clock keeps running.
    pub fn reset(&mut self, next_tick: u64, state: S) {
        self.log = RollbackLog::new(self.log.capacity_ticks());
        self.log.record(next_tick, state, TickInputs::new());
        self.next_tick = next_tick;
        self.confirmed_next = next_tick;
        self.meta.clear();
        self.own.retain(|tick, _| *tick >= next_tick);
    }

    /// One application frame at `now_ms`: steps the frames `seat` has (replaying after a
    /// misprediction) and hands each confirmed tick's state to `confirmed` and its hash to the
    /// seat's vote ([`LockstepSeat::report_hash`]), then steps ahead with predictions to the tick
    /// the seat's clock has reached, at most the window past the frames. `sample` gives the local
    /// input for the capture tick it is called with, once per tick stepped, in order.
    ///
    /// The clock starts with the first frame: the tick it steps is due when it arrives.
    pub fn advance(
        &mut self,
        seat: &mut LockstepSeat<G>,
        now_ms: u64,
        mut sample: impl FnMut(u64) -> G::Input,
        mut confirmed: impl FnMut(u64, &S),
    ) -> RollbackAdvance {
        let _span = tracing::trace_span!("regia_net::rollback_advance").entered();
        let now_us = now_ms.saturating_mul(1000);
        let mut report = RollbackAdvance {
            confirmed: self.confirmed_next..self.confirmed_next,
            ..RollbackAdvance::default()
        };
        // The first tick whose stored inputs were wrong, and the predicted ticks confirmed since
        // the last replay (their states are final once it ran).
        let mut wrong: Option<u64> = None;
        let mut pending: Vec<u64> = Vec::new();
        while let Some(frame) = seat.next_frame() {
            let tick = frame.tick;
            if tick < self.confirmed_next {
                continue;
            }
            self.anchor_us
                .get_or_insert_with(|| now_us.saturating_sub(tick.saturating_mul(self.tick_us)));
            let real: TickInputs<SeatInput<G::Input>> = frame
                .inputs
                .iter()
                .map(|(s, input)| (peer(*s), input.clone()))
                .collect();
            self.note_frame(&frame);
            if tick < self.next_tick {
                let matches = self.log.inputs(tick + 1) == Some(&real)
                    && frame.membership.is_empty()
                    && !frame.match_end;
                if matches && wrong.is_none() {
                    seat.rollback.predictions_confirmed += 1;
                } else {
                    wrong.get_or_insert(tick);
                }
                // In the ring: the log keeps the window plus the confirmed base.
                let _ = self.log.amend_inputs(tick + 1, real);
                pending.push(tick);
            } else {
                // Not stepped yet (the seat waited at the window, or its clock is behind): the
                // ticks before it are settled first, then it is stepped as confirmed.
                self.replay(&mut wrong, seat, &mut report);
                self.flush(&mut pending, seat, &mut confirmed);
                self.capture(seat, tick, &mut sample, &mut report);
                self.step_tick(tick, real);
                self.next_tick = tick + 1;
                self.confirm(tick, seat, &mut confirmed);
            }
            self.confirmed_next = tick + 1;
        }
        report.confirmed.end = self.confirmed_next;
        // The predictions after the newest frame follow from it: one that changes is wrong too.
        for tick in self.confirmed_next..self.next_tick {
            let predicted = self.predict(seat, tick);
            if self.log.inputs(tick + 1) != Some(&predicted) {
                wrong = Some(wrong.map_or(tick, |w| w.min(tick)));
                let _ = self.log.amend_inputs(tick + 1, predicted);
            }
        }
        self.replay(&mut wrong, seat, &mut report);
        self.flush(&mut pending, seat, &mut confirmed);

        // Run ahead to the seat's clock, at most the window past the frames.
        self.sync_clock(seat, now_ms);
        seat.rollback.clock_lead_ticks = self.clock_lead;
        if let Some(anchor) = self.anchor_us {
            let clock =
                i64::try_from(now_us.saturating_sub(anchor) / self.tick_us.max(1)).unwrap_or(i64::MAX);
            let target = u64::try_from(clock.saturating_add(self.clock_lead)).unwrap_or(0);
            while self.next_tick <= target {
                if self.next_tick >= self.confirmed_next + self.window {
                    report.stalled = true;
                    seat.rollback.window_stalls += 1;
                    break;
                }
                let tick = self.next_tick;
                self.capture(seat, tick, &mut sample, &mut report);
                let predicted = self.predict(seat, tick);
                if let Some((capture, _)) = self.own.get(&tick) {
                    let lag = tick - capture;
                    let stats = &mut seat.rollback;
                    stats.local_input_lag_ticks = Some(
                        stats
                            .local_input_lag_ticks
                            .map_or((lag, lag), |(lo, hi)| (lo.min(lag), hi.max(lag))),
                    );
                }
                self.step_tick(tick, predicted);
                self.next_tick = tick + 1;
                report.predicted += 1;
                seat.rollback.ticks_predicted += 1;
            }
        }
        seat.rollback.ahead_ticks = self.ahead_ticks();
        let keep = self.confirmed_next;
        self.own.retain(|tick, _| *tick >= keep);
        self.meta.retain(|tick, _| *tick >= keep);
        report
    }

    /// The newest frame's seats, last inputs and delay, for the predictions after it.
    fn note_frame(&mut self, frame: &LockstepFrame<G::Input>) {
        let mut last = BTreeMap::new();
        for (s, input) in &frame.inputs {
            let value = match input {
                SeatInput::Present(v) | SeatInput::Held(v) => Some(v.clone()),
                SeatInput::Absent => self.last.get(s).cloned().flatten(),
            };
            last.insert(*s, value);
        }
        self.last = last;
        self.delay = (frame.delay, frame.delay_from);
        self.meta.insert(
            frame.tick,
            Meta {
                membership: frame.membership.clone(),
                match_end: frame.match_end,
                delay: frame.delay,
                delay_from: frame.delay_from,
            },
        );
    }

    /// The predicted inputs of `tick`: the seat's own, every other seat's last with its presses
    /// cleared, nothing on a gap tick.
    fn predict(&self, seat: &LockstepSeat<G>, tick: u64) -> TickInputs<SeatInput<G::Input>> {
        let gap = seat.is_gap_tick(tick);
        let rules = seat.input_rules();
        self.last
            .iter()
            .map(|(s, last)| {
                let input = if gap {
                    SeatInput::Absent
                } else if *s == self.seat {
                    self.own
                        .get(&tick)
                        .map_or(SeatInput::Absent, |(_, v)| SeatInput::Present(v.clone()))
                } else {
                    match last {
                        Some(v) => {
                            SeatInput::Present(rules.map_or_else(|| v.clone(), |r| (r.clear_presses)(v)))
                        }
                        None => SeatInput::Absent,
                    }
                };
                (peer(*s), input)
            })
            .collect()
    }

    /// Samples and stamps the local input of capture tick `tick`, once.
    fn capture(
        &mut self,
        seat: &mut LockstepSeat<G>,
        tick: u64,
        sample: &mut impl FnMut(u64) -> G::Input,
        report: &mut RollbackAdvance,
    ) {
        if tick < seat.next_capture() {
            return;
        }
        let input = sample(tick);
        match seat.submit_capture(tick, &input) {
            Ok(Stamp::Tick(stamped)) => {
                // Step what the wire carries, as every other participant will.
                let wire = input.to_bytes().ok().and_then(|b| G::Input::from_bytes(&b).ok());
                self.own.insert(stamped, (tick, wire.unwrap_or(input)));
            }
            Ok(Stamp::Skipped) => {}
            Err(_) => report.inputs_refused += 1,
        }
    }

    /// Steps `tick` from the state before it and stores the state after it with `inputs`.
    fn step_tick(&mut self, tick: u64, inputs: TickInputs<SeatInput<G::Input>>) {
        let Some(before) = self.log.state(tick) else {
            return;
        };
        let mut state = before.clone();
        state.step(&frame_of(tick, &inputs, self.meta.get(&tick), self.delay));
        self.log.record(tick + 1, state, inputs);
    }

    /// Replays from the first wrong tick through the newest tick stepped.
    fn replay(&mut self, wrong: &mut Option<u64>, seat: &mut LockstepSeat<G>, report: &mut RollbackAdvance) {
        let Some(from) = wrong.take() else {
            return;
        };
        let Some(base) = self.log.state(from).cloned() else {
            return;
        };
        let (meta, delay) = (&self.meta, self.delay);
        let replayed = self.log.reconcile(from, base, |state, log_tick, inputs| {
            let tick = log_tick - 1;
            state.step(&frame_of(tick, inputs, meta.get(&tick), delay));
        });
        if replayed.is_ok() {
            let depth = self.next_tick - from;
            let stats = &mut seat.rollback;
            stats.rollbacks += 1;
            stats.rollback_ticks_max = stats.rollback_ticks_max.max(depth);
            stats.rollback_ticks_replayed += depth;
            report.rolled_back = Some(report.rolled_back.map_or(depth, |d| d.max(depth)));
        }
    }

    /// Hands out the confirmed ticks whose states are final.
    fn flush(
        &mut self,
        pending: &mut Vec<u64>,
        seat: &mut LockstepSeat<G>,
        confirmed: &mut impl FnMut(u64, &S),
    ) {
        for tick in pending.drain(..) {
            self.confirm(tick, seat, confirmed);
        }
    }

    fn confirm(&self, tick: u64, seat: &mut LockstepSeat<G>, confirmed: &mut impl FnMut(u64, &S)) {
        if let Some(state) = self.log.state(tick + 1) {
            seat.report_hash(tick, state.hash());
            confirmed(tick, state);
        }
    }
}

/// The frame a stored tick steps with: a confirmed frame's own fields, or none for a predicted one.
fn frame_of<I: Clone>(
    tick: u64,
    inputs: &TickInputs<SeatInput<I>>,
    meta: Option<&Meta>,
    delay: (u16, u64),
) -> LockstepFrame<I> {
    LockstepFrame {
        tick,
        inputs: inputs
            .iter()
            .map(|(p, input)| (seat_of(*p), input.clone()))
            .collect(),
        delay: meta.map_or(delay.0, |m| m.delay),
        delay_from: meta.map_or(delay.1, |m| m.delay_from),
        membership: meta.map_or_else(Vec::new, |m| m.membership.clone()),
        match_end: meta.is_some_and(|m| m.match_end),
    }
}

#[cold]
fn unreachable_state<'a, S>() -> &'a S {
    // The log always holds the base tick: `new` and `reset` record it and every step records
    // the next one, so the ring is never empty.
    panic!("rollback runner log is empty")
}
