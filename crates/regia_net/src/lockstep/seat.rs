// SPDX-License-Identifier: MPL-2.0
//! A lockstep seat: stamps its inputs `capture tick + delay` and steps only from frames.

use std::collections::{BTreeMap, VecDeque};

use super::host::{HASH_RING_TICKS, held_input, put_ring_hash};
use super::loss::{LossRuns, SeqClass};
use super::migration::{self, MigrationRoster, RttMatrix};
use super::quorum::Voter;
use super::stamp::{Stamp, Stamper};
use super::wire::{
    self, FrameState, InputsMsg, KIND_CATCHUP, KIND_FRAMES, KIND_ORDER, KIND_STATE_CHUNK, KIND_STREAM_FRAMES,
    MAX_INPUTS_PER_DATAGRAM, MAX_VOTES_PER_DATAGRAM, Order, SeatStream,
};
use super::{
    DelayPolicy, InputRules, Lane, LockstepConfig, LockstepError, LockstepFrame, LockstepStatus,
    MembershipKind, Outgoing, SeatId, SeatInput,
};
use crate::desync::{DesyncHash, ProbeRows, SubsystemRing};
use crate::feedback::{LEAD_CALM_MS, LinkFeedback, TARGET_MARGIN_TICKS};
use crate::payload::{NetCodec, NetGame};

/// Checkpoint votes kept until the host acknowledges them.
const MAX_PENDING_VOTES: usize = 16;
/// Stamped inputs kept at most (a host that never builds cannot grow the map).
const MAX_STAMPED: usize = 512;
/// Stream payloads held back while the application is not stepping (the host stops handing frames
/// to the stream one frame log past the seat's acknowledgement, so this is a backstop).
const MAX_STREAM_BACKLOG: usize = 1024;
/// A catching-up seat is at the live edge when at most this many frames it knows of are unstepped.
const CAUGHT_UP_SLACK_TICKS: u64 = 2;

/// Something the seat's application has to know or do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatEvent {
    /// The input delay changes at capture tick `delay_from`.
    DelayChanged {
        /// The new delay, ticks.
        delay: u16,
        /// First capture tick it applies to.
        delay_from: u64,
    },
    /// The host asks for a self-check: replay from `from` (the last agreed checkpoint; `None`:
    /// the start) through `tick` with [`LockstepSeat::frames_since`], then
    /// [`LockstepSeat::revote`]. A game that cannot replay ignores it.
    SelfCheck {
        /// The agreed checkpoint to replay from.
        from: Option<u64>,
        /// The checkpoint to vote on again.
        tick: u64,
    },
    /// This seat's state differs from the reference: take `source`'s state at `tick`, replay the
    /// frames after it, then call [`LockstepSeat::resynced`].
    Resync {
        /// The checkpoint tick of the state.
        tick: u64,
        /// Whose state to take.
        source: Voter,
    },
    /// Send this seat's state at `tick` to `target` (this seat votes with the majority).
    ProvideState {
        /// The checkpoint tick of the state.
        tick: u64,
        /// Who needs it.
        target: Voter,
    },
    /// The seat was removed from the match for repeated divergence (`desync`).
    Removed {
        /// The checkpoint that decided it.
        tick: u64,
    },
    /// The votes cannot be decided: the match ends with `desync_unresolved`, without blame.
    Unresolved {
        /// The checkpoint that could not be decided.
        tick: u64,
    },
    /// The game's state of a catch-up arrived: take it with [`LockstepSeat::take_state`], load
    /// it as the state after `tick`, then step the frames [`LockstepSeat::next_frame`] hands out.
    StateReady {
        /// The tick the state is after.
        tick: u64,
    },
    /// The seat has stepped a catch-up to the live edge: its captures count from `tick` on.
    CaughtUp {
        /// The open capture tick.
        tick: u64,
    },
    /// The frame at `tick` lists this seat's join: it is a member from that tick on.
    Joined {
        /// The frame of the join.
        tick: u64,
    },
    /// The frame at `tick` lists this seat's leave (it asked, or the host removed it).
    Left {
        /// The frame of the leave.
        tick: u64,
    },
    /// The frame at `tick` is the last of the match.
    MatchEnded {
        /// The last frame.
        tick: u64,
    },
}

/// A catch-up being received: the game's state arrives in pieces after the head.
#[derive(Debug)]
struct Incoming {
    state_tick: u64,
    state: Vec<u8>,
    len: usize,
}

/// One seat of a lockstep match (spec 120 § 7.1).
///
/// Drive it once per application frame: [`Self::receive`] what the host sent, then for each
/// [`Self::next_frame`] step the simulation, [`Self::report_hash`] and [`Self::submit_input`];
/// finally send everything [`Self::outgoing`] returns.
pub struct LockstepSeat<G: NetGame> {
    config: LockstepConfig,
    seat: SeatId,
    state: FrameState,
    /// The next frame tick needed: every earlier frame arrived.
    next_expected: u64,
    ready: VecDeque<LockstepFrame<G::Input>>,
    /// Stream payloads that arrived while `ready` was full: applied, in order, as it drains.
    stream_backlog: VecDeque<Vec<u8>>,
    /// The next tick to hand out: the open capture tick.
    next_step: u64,
    log: VecDeque<LockstepFrame<G::Input>>,
    stamper: Stamper,
    /// Stamped inputs the host has not spent yet, by tick.
    stamped: BTreeMap<u64, Vec<u8>>,
    /// The newest stamped tick that has been sent at least once.
    sent_through: Option<u64>,
    host_next_build: u64,
    down: LossRuns,
    out_seq: u64,
    votes: VecDeque<(u64, u64)>,
    hash_ring: VecDeque<(u64, u64)>,
    /// Subsystem rows of the last ticks stepped (spec 120 § 7.6).
    rows: SubsystemRing,
    /// The rows of the window the host's last probe asked about, kept as they were then: a state
    /// transfer ordered by the same verdict rewrites the ring before the second stage asks.
    probe_hold: Option<ProbeRows>,
    stream_out: Vec<Vec<u8>>,
    feedback: Option<LinkFeedback>,
    extra_lead: u16,
    /// `(first stamped tick, extra lead)` runs, for the margin correction.
    lead_runs: VecDeque<(u64, u16)>,
    calm_since_ms: Option<u64>,
    events: VecDeque<SeatEvent>,
    frames_repaired: u64,
    repair_ticks_max: u64,
    single_losses: u64,
    single_loss_repair_ticks_max: u64,
    /// A single datagram was lost while the chain was whole: the first frame the seat lacked
    /// then. Closed when that frame has arrived and the chain is whole again.
    single_loss_from: Option<u64>,
    /// The longest repair among the frames of the datagram being applied.
    repair_ticks_last: u64,
    /// The newest frames payload started past the frames this seat has: the host is asked for
    /// every unacknowledged frame.
    hole: bool,
    stream_frames: u64,
    undecodable: u64,
    datagrams_sent: u64,
    datagram_bytes_sent: u64,
    stream_bytes_sent: u64,
    fp_env_corrections: u64,
    removed: bool,
    rules: Option<InputRules<G::Input>>,
    /// The seat is listed in the frames.
    member: bool,
    /// The seat waits for a catch-up's head: frames before it cannot be decoded.
    awaiting_head: bool,
    /// The seat is stepping a catch-up's frames.
    catching_up: bool,
    incoming: Option<Incoming>,
    /// A complete catch-up state the application has not taken.
    state_ready: Option<(u64, Vec<u8>)>,
    /// The application took the catch-up state: frames may be handed out.
    state_taken: bool,
    /// Frames handed out since the last `outgoing` (the catch-up budget).
    handed: u32,
    leave_requested: bool,
    ended_at: Option<u64>,
    catchups: u64,
    /// What a [`RollbackRunner`](super::RollbackRunner) driving this seat counted.
    pub(crate) rollback: RollbackStats,
    /// The frame cache of host migration (spec 120 § 10.5): the bodies of the frames
    /// `cache_front..next_expected`, at most `frame_log_ticks` of them.
    cache: VecDeque<Vec<u8>>,
    cache_front: u64,
    /// The delta base before `cache_front`.
    cache_base: FrameState,
    /// When anything of the host last arrived.
    host_heard_ms: Option<u64>,
    /// The migration roster the host shared, with a revision bumped on each one.
    roster: Option<(u64, MigrationRoster)>,
    /// The newest round-trip matrix the host shared.
    matrix: Option<RttMatrix>,
}

/// The counters of rollback lockstep (spec 120 § 7.7) a runner keeps on its seat, for
/// `net.lockstep`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RollbackStats {
    pub(crate) rollbacks: u64,
    pub(crate) rollback_ticks_max: u64,
    pub(crate) rollback_ticks_replayed: u64,
    pub(crate) ticks_predicted: u64,
    pub(crate) predictions_confirmed: u64,
    pub(crate) window_stalls: u64,
    pub(crate) ahead_ticks: u64,
    pub(crate) local_input_lag_ticks: Option<(u64, u64)>,
    pub(crate) clock_lead_ticks: i64,
}

impl<G: NetGame> std::fmt::Debug for LockstepSeat<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockstepSeat")
            .field("seat", &self.seat)
            .field("next_expected", &self.next_expected)
            .field("next_step", &self.next_step)
            .finish_non_exhaustive()
    }
}

impl<G: NetGame> LockstepSeat<G> {
    /// A seat with `config` (sanitized; the same values as the host's) that joins at tick 0.
    pub fn new(config: LockstepConfig, seat: SeatId) -> Self {
        let config = config.sanitized();
        Self {
            seat,
            state: FrameState::new(config.initial_delay_ticks),
            next_expected: 0,
            ready: VecDeque::new(),
            stream_backlog: VecDeque::new(),
            next_step: 0,
            log: VecDeque::new(),
            stamper: Stamper::new(config.initial_delay_ticks),
            stamped: BTreeMap::new(),
            sent_through: None,
            host_next_build: 0,
            down: LossRuns::with_interval_us(config.tick_us()),
            out_seq: 0,
            votes: VecDeque::new(),
            hash_ring: VecDeque::with_capacity(HASH_RING_TICKS),
            rows: SubsystemRing::new(),
            probe_hold: None,
            stream_out: Vec::new(),
            feedback: None,
            extra_lead: 0,
            lead_runs: VecDeque::new(),
            calm_since_ms: None,
            events: VecDeque::new(),
            frames_repaired: 0,
            repair_ticks_max: 0,
            single_losses: 0,
            single_loss_repair_ticks_max: 0,
            single_loss_from: None,
            repair_ticks_last: 0,
            hole: false,
            stream_frames: 0,
            undecodable: 0,
            datagrams_sent: 0,
            datagram_bytes_sent: 0,
            stream_bytes_sent: 0,
            fp_env_corrections: 0,
            removed: false,
            rules: None,
            member: true,
            awaiting_head: false,
            catching_up: false,
            incoming: None,
            state_ready: None,
            state_taken: true,
            handed: 0,
            leave_requested: false,
            ended_at: None,
            catchups: 0,
            rollback: RollbackStats::default(),
            cache: VecDeque::new(),
            cache_front: 0,
            cache_base: FrameState::new(config.initial_delay_ticks),
            host_heard_ms: None,
            roster: None,
            matrix: None,
            config,
        }
    }

    /// A seat that joins a running match, or returns to it, through a catch-up (spec 120 § 7.4):
    /// it takes no frame until the host's [`LockstepHost::catch_up`](super::LockstepHost::catch_up)
    /// arrives, then hands out the frames after the state's tick, at most
    /// [`LockstepConfig::catchup_steps_per_frame`] per application frame, and joins (or resumes
    /// its seat) once it has stepped to the live edge.
    pub fn catching_up(config: LockstepConfig, seat: SeatId) -> Self {
        let mut s = Self::new(config, seat);
        s.member = false;
        s.awaiting_head = true;
        s.catching_up = true;
        s
    }

    /// Sets the game's rules for missing inputs; the same rules as the host's
    /// [`LockstepHost::set_input_rules`](super::LockstepHost::set_input_rules).
    pub fn set_input_rules(&mut self, rules: InputRules<G::Input>) {
        self.rules = Some(rules);
    }

    /// Asks the host to take this seat out of the match. Every seat sees the leave on the same
    /// frame ([`SeatEvent::Left`]); the seat keeps stepping until it.
    pub fn leave(&mut self) {
        self.leave_requested = true;
    }

    /// Whether the seat is listed in the frames.
    pub fn is_member(&self) -> bool {
        self.member
    }

    /// Whether the seat is stepping a catch-up and not at the live edge yet.
    pub fn is_catching_up(&self) -> bool {
        self.catching_up
    }

    /// The tick of the frame that ended the match.
    pub fn ended_at(&self) -> Option<u64> {
        self.ended_at
    }

    /// The game's state a catch-up carried, as `(tick, bytes)`: load it as the state after
    /// `tick` before stepping the frames that follow. `None` until [`SeatEvent::StateReady`].
    pub fn take_state(&mut self) -> Option<(u64, Vec<u8>)> {
        let state = self.state_ready.take()?;
        self.state_taken = true;
        Some(state)
    }

    /// Starts a catch-up from its head: the frame chain restarts after `state_tick`.
    fn begin_catchup(&mut self, head: wire::CatchupHead) {
        let next = head.state_tick.saturating_add(1);
        self.member = head.base.seats.contains_key(&self.seat);
        self.stamper = Stamper::resume_at(next, head.base.delay, head.base.delay_from);
        self.cache.clear();
        self.cache_front = next;
        self.cache_base = head.base.clone();
        self.state = head.base;
        self.next_expected = next;
        self.next_step = next;
        self.ready.clear();
        self.stream_backlog.clear();
        self.log.clear();
        self.stamped.clear();
        self.sent_through = None;
        self.votes.clear();
        self.lead_runs.clear();
        self.hole = false;
        self.single_loss_from = None;
        // A new session of the seat: an earlier leave request, the per-tick hashes of the old
        // chain and the datagram sequence of the old connection do not carry over.
        self.leave_requested = false;
        self.removed = false;
        self.hash_ring.clear();
        self.down = LossRuns::with_interval_us(self.config.tick_us());
        self.awaiting_head = false;
        self.catching_up = true;
        self.state_taken = false;
        self.state_ready = None;
        self.catchups += 1;
        self.incoming = Some(Incoming {
            state_tick: head.state_tick,
            state: Vec::with_capacity(head.state_len.min(64 << 10)),
            len: head.state_len,
        });
        self.finish_state();
    }

    fn finish_state(&mut self) {
        if self.incoming.as_ref().is_some_and(|i| i.state.len() >= i.len)
            && let Some(done) = self.incoming.take()
        {
            self.events.push_back(SeatEvent::StateReady {
                tick: done.state_tick,
            });
            self.state_ready = Some((done.state_tick, done.state));
        }
    }

    /// A catching-up seat that has stepped what it knows of the live edge is in step.
    fn check_caught_up(&mut self) {
        if self.catching_up
            && !self.awaiting_head
            && self.state_taken
            && self.incoming.is_none()
            && self.ready.len() as u64 <= CAUGHT_UP_SLACK_TICKS
            && self.next_step + CAUGHT_UP_SLACK_TICKS >= self.host_next_build
        {
            self.catching_up = false;
            self.events
                .push_back(SeatEvent::CaughtUp { tick: self.next_step });
        }
    }

    /// This seat's id.
    pub fn seat(&self) -> SeatId {
        self.seat
    }

    /// The settings in force.
    pub fn config(&self) -> &LockstepConfig {
        &self.config
    }

    /// The next tick [`Self::next_frame`] hands out: the open capture tick.
    pub fn tick(&self) -> u64 {
        self.next_step
    }

    /// The next frame tick the seat needs from the host (what it acknowledges).
    pub fn frames_received_through(&self) -> u64 {
        self.next_expected
    }

    /// The input delay for the open capture tick, ticks.
    pub fn delay_ticks(&self) -> u16 {
        self.stamper.delay_at(self.next_step)
    }

    /// Ticks this seat stamps ahead of the frame delay ([`DelayPolicy::PerSeat`]).
    pub fn extra_lead_ticks(&self) -> u16 {
        self.extra_lead
    }

    /// The host's last input timing record for this seat.
    pub fn link_feedback(&self) -> Option<&LinkFeedback> {
        self.feedback.as_ref()
    }

    /// Whether the host removed this seat.
    pub fn removed(&self) -> bool {
        self.removed
    }

    /// Takes one lockstep payload the host sent on `lane`.
    pub fn receive(&mut self, lane: Lane, bytes: &[u8], now_ms: u64) -> Result<(), LockstepError> {
        self.host_heard_ms = Some(now_ms);
        match (lane, wire::kind_of(bytes)) {
            (Lane::Stream, Some(migration::KIND_ROSTER)) => {
                let roster =
                    migration::decode_roster(bytes).map_err(|_| LockstepError::Malformed("roster"))?;
                let rev = self.roster.as_ref().map_or(1, |(rev, _)| rev + 1);
                self.roster = Some((rev, roster));
                Ok(())
            }
            (Lane::Stream, Some(migration::KIND_RTT)) => {
                let matrix =
                    migration::decode_rtt(bytes).map_err(|_| LockstepError::Malformed("rtt matrix"))?;
                if self.matrix.as_ref().is_none_or(|m| matrix.version > m.version) {
                    self.matrix = Some(matrix);
                }
                Ok(())
            }
            (Lane::Datagram, Some(KIND_FRAMES)) => {
                let (header, bodies) = wire::decode_frames_datagram(bytes)?;
                if self.awaiting_head {
                    // The chain starts at the catch-up: nothing before it decodes.
                    self.host_next_build = self.host_next_build.max(header.next_build);
                    return Ok(());
                }
                // One missing datagram before this one, with the frame chain whole until then:
                // the seat had every frame the host had told it of. (The hole flag is not that:
                // a late datagram that extends the chain by one frame clears it while newer
                // frames are still missing, and a loss that follows is not a single loss.)
                let whole = self.next_expected >= self.host_next_build;
                let single_loss = self.down.on_seq(header.seq, now_ms) == SeqClass::AfterLoss(1) && whole;
                self.host_next_build = self.host_next_build.max(header.next_build);
                if let Some(ack) = header.vote_ack {
                    while self.votes.front().is_some_and(|(tick, _)| *tick <= ack) {
                        self.votes.pop_front();
                    }
                }
                if let Some(fb) = header.feedback {
                    self.on_feedback(fb, now_ms);
                }
                self.repair_ticks_last = 0;
                let lacked = self.next_expected;
                let result = self.apply(header.base_tick, &bodies, false);
                if single_loss {
                    self.single_loss_from.get_or_insert(lacked);
                }
                // The loss is repaired once the frame the seat lacked is here and the chain is
                // whole. Its stall is how far the newest frame is ahead of that one: 1 when the
                // next datagram repaired it, more when it did not reach back and the seat had
                // to report a hole.
                if let Some(from) = self.single_loss_from
                    && !self.hole
                    && self.next_expected > from
                {
                    self.single_loss_from = None;
                    self.single_losses += 1;
                    self.single_loss_repair_ticks_max = self
                        .single_loss_repair_ticks_max
                        .max((self.next_expected - 1).saturating_sub(from));
                }
                result
            }
            (Lane::Stream, Some(KIND_STREAM_FRAMES)) => {
                let (base_tick, bodies) = wire::decode_stream_frames(bytes)?;
                // The stream delivers once: a payload the application has no room for is kept, not
                // dropped, and applied when frames are stepped.
                if !self.stream_backlog.is_empty() || self.ready.len() >= self.config.frame_log_ticks {
                    if self.stream_backlog.len() < MAX_STREAM_BACKLOG {
                        self.stream_backlog.push_back(bytes.to_vec());
                    } else {
                        self.undecodable += 1;
                    }
                    return Ok(());
                }
                self.apply(base_tick, &bodies, true)
            }
            (Lane::Stream, Some(KIND_ORDER)) => {
                self.on_order(wire::decode_order(bytes)?);
                Ok(())
            }
            (Lane::Stream, Some(KIND_CATCHUP)) => {
                let head = wire::decode_catchup(bytes, self.config.max_input_bytes)?;
                self.begin_catchup(head);
                Ok(())
            }
            (Lane::Stream, Some(KIND_STATE_CHUNK)) => {
                let Some(incoming) = &mut self.incoming else {
                    return Err(LockstepError::Malformed("state piece without a catch-up"));
                };
                let piece = bytes.get(1..).unwrap_or_default();
                if incoming.state.len() + piece.len() > incoming.len {
                    self.incoming = None;
                    return Err(LockstepError::Malformed("state longer than its catch-up said"));
                }
                incoming.state.extend_from_slice(piece);
                self.finish_state();
                Ok(())
            }
            _ => Err(LockstepError::Malformed("unexpected payload kind for the lane")),
        }
    }

    /// Applies the frames of one payload that continue the chain; older ones are stepped over and
    /// a payload that starts past the chain is left for the stream to fill in.
    fn apply(&mut self, base_tick: u64, bodies: &[&[u8]], stream: bool) -> Result<(), LockstepError> {
        let Some(last) = bodies.len().checked_sub(1) else {
            return Ok(());
        };
        let newest = base_tick.saturating_add(last as u64);
        // An application that stopped stepping takes no more frames from datagrams: they stay
        // unacknowledged at the host instead of growing a queue here. It has no hole to report
        // either: a repair would send it every unacknowledged frame, tick after tick, and it
        // would take none of them. The real hole shows when it steps again.
        if !stream && self.ready.len() >= self.config.frame_log_ticks {
            self.hole = false;
            return Ok(());
        }
        if base_tick > self.next_expected {
            // Frames are missing that this payload does not repeat: tell the host. Under
            // interleaved redundancy that is every loss run of two or more whose first datagram
            // after it is a shallow one; the run is then repaired by whichever comes first, the
            // next deeper datagram or the host's answer to this report.
            self.undecodable += 1;
            self.hole = true;
            return Ok(());
        }
        if newest < self.next_expected {
            return Ok(());
        }
        // Decode against a copy: a payload with a bad frame leaves the chain untouched.
        let mut state = self.state.clone();
        let mut frames = Vec::new();
        let mut epochs = Vec::new();
        let mut cached = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            let tick = base_tick + i as u64;
            if tick < self.next_expected {
                continue;
            }
            let raw = wire::decode_frame_body(body, tick, &mut state, self.config.max_input_bytes)?;
            cached.push(body.to_vec());
            if let Some(change) = raw.delay_change {
                epochs.push(change);
            }
            let mut inputs = Vec::with_capacity(state.seats.len());
            if raw.gap {
                inputs.extend(state.seats.keys().map(|seat| (*seat, SeatInput::Absent)));
            } else {
                for (seat, entry) in &raw.entries {
                    inputs.push((
                        *seat,
                        match entry {
                            Some(bytes) => SeatInput::Present(G::Input::from_bytes(bytes)?),
                            None => held_input::<G::Input>(
                                self.config.missing_input,
                                self.rules,
                                state.seats.get(seat),
                            )
                            .map_or(SeatInput::Absent, SeatInput::Held),
                        },
                    ));
                }
            }
            frames.push(LockstepFrame {
                tick,
                inputs,
                delay: state.delay,
                delay_from: state.delay_from,
                membership: raw.membership,
                match_end: raw.match_end,
            });
        }
        self.state = state;
        for frame in &frames {
            if stream {
                self.stream_frames += 1;
            } else if frame.tick < newest {
                self.frames_repaired += 1;
                self.repair_ticks_max = self.repair_ticks_max.max(newest - frame.tick);
                self.repair_ticks_last = self.repair_ticks_last.max(newest - frame.tick);
            }
        }
        self.next_expected += frames.len() as u64;
        self.hole = false;
        self.ready.extend(frames);
        self.cache_bodies(cached);
        for (delay, delay_from) in epochs {
            if self.stamper.set_delay(delay_from, delay) {
                self.events
                    .push_back(SeatEvent::DelayChanged { delay, delay_from });
            }
        }
        Ok(())
    }

    fn on_order(&mut self, order: Order) {
        let voter = |v: Option<SeatId>| v.map_or(Voter::Observer, Voter::Seat);
        match order {
            Order::Recheck { tick, from } => self.events.push_back(SeatEvent::SelfCheck { from, tick }),
            Order::Resync { tick, source } => self.events.push_back(SeatEvent::Resync {
                tick,
                source: voter(source),
            }),
            Order::ProvideState { tick, target } => self.events.push_back(SeatEvent::ProvideState {
                tick,
                target: voter(target),
            }),
            Order::Probe { from, to } => {
                let hashes: Vec<u64> = (from..=to)
                    .map_while(|t| {
                        self.hash_ring
                            .iter()
                            .find(|(tick, _)| *tick == t)
                            .map(|(_, h)| *h)
                    })
                    .collect();
                self.stream_out
                    .push(wire::encode_seat_stream(&SeatStream::ProbeRows { from, hashes }));
                self.probe_hold = (!self.rows.is_empty()).then(|| self.rows.window(from, to));
            }
            Order::ProbeSubsystems { from, to } => {
                let held = self
                    .probe_hold
                    .as_ref()
                    .filter(|held| (from..=to).all(|t| held.rows.iter().any(|row| row.tick == t)));
                let rows = match held {
                    Some(held) => ProbeRows {
                        names: held.names.clone(),
                        rows: held
                            .rows
                            .iter()
                            .filter(|row| (from..=to).contains(&row.tick))
                            .cloned()
                            .collect(),
                    },
                    None => self.rows.window(from, to),
                };
                self.stream_out
                    .push(wire::encode_seat_stream(&SeatStream::SubsystemRows(rows)));
            }
            Order::Removed { tick } => {
                self.removed = true;
                self.events.push_back(SeatEvent::Removed { tick });
            }
            Order::Unresolved { tick } => self.events.push_back(SeatEvent::Unresolved { tick }),
        }
    }

    /// The seat's own lead under [`DelayPolicy::PerSeat`], steered from the host's record like
    /// the client's input lead (spec 120 § 9.2): a p5 margin under the target raises the lead at
    /// once by the deficit, and five calm seconds lower it by one. The reported margin is
    /// corrected by how much further ahead the seat stamps now than it did for the input that
    /// set the p5, so one late spell does not raise the lead once per record.
    fn on_feedback(&mut self, fb: LinkFeedback, now_ms: u64) {
        self.feedback = Some(fb);
        if self.config.delay_policy != DelayPolicy::PerSeat || fb.margin_samples == 0 {
            return;
        }
        let lead_then = fb
            .margin_p5_tick
            .and_then(|tick| {
                self.lead_runs
                    .iter()
                    .rev()
                    .find(|(first, _)| *first <= tick)
                    .map(|(_, lead)| *lead)
            })
            .unwrap_or(self.extra_lead);
        let corrected = fb.margin_p5 + i32::from(self.extra_lead) - i32::from(lead_then);
        let deficit = TARGET_MARGIN_TICKS - corrected;
        if deficit > 0 {
            let raise = u16::try_from(deficit).unwrap_or(u16::MAX);
            self.extra_lead = self
                .extra_lead
                .saturating_add(raise)
                .min(self.config.max_extra_lead_ticks);
            self.calm_since_ms = None;
        } else if corrected > TARGET_MARGIN_TICKS {
            let since = *self.calm_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= LEAD_CALM_MS && self.extra_lead > 0 {
                self.extra_lead -= 1;
                self.calm_since_ms = Some(now_ms);
            }
        } else {
            self.calm_since_ms = None;
        }
    }

    /// The next frame to step, in tick order, each exactly once; `None` until it arrives.
    pub fn next_frame(&mut self) -> Option<LockstepFrame<G::Input>> {
        if self.catching_up && (!self.state_taken || self.handed >= self.config.catchup_steps_per_frame) {
            return None;
        }
        let frame = self.ready.pop_front()?;
        self.handed = self.handed.saturating_add(1);
        self.next_step = frame.tick + 1;
        for change in frame.membership.iter().filter(|c| c.seat == self.seat) {
            match change.kind {
                MembershipKind::Join => {
                    self.member = true;
                    self.events.push_back(SeatEvent::Joined { tick: frame.tick });
                }
                MembershipKind::Leave => {
                    self.member = false;
                    self.leave_requested = false;
                    self.events.push_back(SeatEvent::Left { tick: frame.tick });
                }
            }
        }
        if frame.match_end {
            self.ended_at = Some(frame.tick);
            self.events.push_back(SeatEvent::MatchEnded { tick: frame.tick });
        }
        if self.log.len() >= self.config.frame_log_ticks {
            self.log.pop_front();
        }
        self.log.push_back(frame.clone());
        // Room again: the stream payloads that were held back continue the chain.
        while self.ready.len() < self.config.frame_log_ticks {
            let Some(bytes) = self.stream_backlog.pop_front() else {
                break;
            };
            let applied = wire::decode_stream_frames(&bytes)
                .and_then(|(base_tick, bodies)| self.apply(base_tick, &bodies, true));
            if applied.is_err() {
                self.undecodable += 1;
            }
        }
        self.check_caught_up();
        // The simulation steps next: its thread's float environment must be the default.
        if !regia_detsim::fpu::ensure_default_fp_env().was_default() {
            self.fp_env_corrections += 1;
        }
        Some(frame)
    }

    /// Frames received and not stepped yet.
    pub fn frames_waiting(&self) -> usize {
        self.ready.len()
    }

    /// The frames already stepped from `from_tick` on that the seat still holds, oldest first:
    /// what a self-check or a state transfer replays.
    pub fn frames_since(&self, from_tick: u64) -> impl Iterator<Item = &LockstepFrame<G::Input>> {
        self.log.iter().filter(move |f| f.tick >= from_tick)
    }

    /// The input sampled at the open capture tick (the next tick to step). It is stamped
    /// `capture tick + delay` (plus the seat's own lead) and sent until the host has built that
    /// tick. One input per capture tick: a second call before the next frame is stepped is
    /// refused with [`LockstepError::CaptureClosed`]. [`Stamp::Skipped`] means the delay was
    /// lowered and this capture has no tick of its own.
    ///
    /// Under [`LockstepMode::Rollback`](super::LockstepMode::Rollback) the capture tick runs
    /// ahead of the frames: drive the seat with a [`RollbackRunner`](super::RollbackRunner),
    /// which samples one input per tick it steps.
    pub fn submit_input(&mut self, input: &G::Input) -> Result<Stamp, LockstepError> {
        self.stamp_input(input, self.next_step)
    }

    /// The input sampled at capture tick `capture`, for a rollback runner: the captures before it
    /// keep their place without an input. `capture` may run up to the rollback window past the
    /// frames the seat has; an earlier or a taken capture is [`LockstepError::CaptureClosed`].
    pub(crate) fn submit_capture(&mut self, capture: u64, input: &G::Input) -> Result<Stamp, LockstepError> {
        if capture < self.stamper.next_capture() {
            return Err(LockstepError::CaptureClosed);
        }
        self.stamp_input(input, capture)
    }

    /// Stamps `input` for the capture tick `capture` (the open one: the next tick to step, or a
    /// rollback seat's tick ahead of its frames).
    fn stamp_input(&mut self, input: &G::Input, capture: u64) -> Result<Stamp, LockstepError> {
        if self.ended_at.is_some() {
            return Err(LockstepError::MatchEnded);
        }
        // A strict seat captures the next tick to step only; a rollback seat up to its window
        // past the frames it has.
        let ahead = u64::from(self.config.mode.window_ticks()).saturating_sub(1);
        if self.stamper.next_capture() > self.next_step.saturating_add(ahead)
            || capture > self.next_step + ahead
        {
            return Err(LockstepError::CaptureClosed);
        }
        // A seat that is catching up or not listed has no tick to stamp: its frames are history.
        if self.catching_up || !self.member {
            return Ok(Stamp::Skipped);
        }
        let bytes = input.to_bytes()?;
        if bytes.len() > self.config.max_input_bytes {
            return Err(LockstepError::InputTooLarge {
                len: bytes.len(),
                max: self.config.max_input_bytes,
            });
        }
        // Captures the application let pass keep their place in the schedule, without an input.
        while self.stamper.next_capture() < capture {
            self.stamper.stamp_next(self.extra_lead);
        }
        let (_, stamp) = self.stamper.stamp_next(self.extra_lead);
        if let Stamp::Tick(tick) = stamp {
            if self
                .lead_runs
                .back()
                .is_none_or(|(_, lead)| *lead != self.extra_lead)
            {
                if self.lead_runs.len() >= 64 {
                    self.lead_runs.pop_front();
                }
                self.lead_runs.push_back((tick, self.extra_lead));
            }
            if self.stamped.len() >= MAX_STAMPED {
                self.stamped.pop_first();
            }
            self.stamped.insert(tick, bytes);
        }
        Ok(stamp)
    }

    /// The hash of this seat's simulation after it stepped `tick`. Report it every tick: the
    /// checkpoint ticks are votes, the rest answer the host's probe for the first diverged tick.
    /// A tick reported again replaces its hash in the probe's ring.
    pub fn report_hash(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
        if self.config.hash_every > 0 && tick.is_multiple_of(u64::from(self.config.hash_every)) {
            if self.votes.len() >= MAX_PENDING_VOTES {
                self.votes.pop_front();
            }
            self.votes.push_back((tick, hash));
        }
    }

    /// Hashes the simulation after it stepped `tick` live ([`DesyncHash`]): the subsystem rows go
    /// into the seat's ring (they travel only when the host probes), and the world hash is reported
    /// as by [`Self::report_hash`]. Returns the world hash. Use it instead of `report_hash`.
    pub fn record_state<H: DesyncHash + ?Sized>(&mut self, tick: u64, state: &H) -> u64 {
        let world = self.rows.record(tick, state);
        self.report_hash(tick, world);
        world
    }

    /// Hashes the simulation after it stepped `tick` again in a replay (a self-check or after a
    /// state transfer): the ring's rows from `tick` on are replaced and the world hash replaces
    /// `tick`'s in the probe's hash ring, no vote is cast. Returns the world hash, the value for
    /// [`Self::revote`] or [`Self::resynced`] at a checkpoint.
    pub fn record_replayed<H: DesyncHash + ?Sized>(&mut self, tick: u64, state: &H) -> u64 {
        let world = self.rows.record(tick, state);
        put_ring_hash(&mut self.hash_ring, tick, world);
        world
    }

    /// The hash of the simulation after it stepped `tick` again in a replay (a
    /// [`SeatEvent::SelfCheck`], or the frames after a [`SeatEvent::Resync`] state), for a game
    /// that reports plain hashes ([`Self::report_hash`]) rather than [`Self::record_replayed`]: it
    /// replaces `tick`'s hash in the probe's ring and casts no vote. Report every replayed tick, so
    /// the host's next probe gets the history the simulation has now (ADR-10147).
    pub fn report_replayed_hash(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
    }

    /// The subsystem rows this seat keeps.
    pub fn subsystem_rows(&self) -> &SubsystemRing {
        &self.rows
    }

    /// This seat's vote on `tick` again, after the replay a [`SeatEvent::SelfCheck`] asked for.
    /// The hash also replaces `tick`'s in the probe's ring.
    pub fn revote(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
        self.stream_out
            .push(wire::encode_seat_stream(&SeatStream::Revote {
                tick,
                round: 1,
                hash,
            }));
    }

    /// The seat took the state a [`SeatEvent::Resync`] named and replayed: `hash` is its hash at
    /// the checkpoint `tick`. The hash also replaces `tick`'s in the probe's ring; the replayed
    /// ticks after it go in with [`Self::record_replayed`] or [`Self::report_replayed_hash`].
    pub fn resynced(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
        self.stream_out
            .push(wire::encode_seat_stream(&SeatStream::Revote {
                tick,
                round: 2,
                hash,
            }));
    }

    /// The payloads to send now: one inputs datagram (every stamped input the host has not built
    /// yet, the frame acknowledgement, pending votes) and any stream replies. Call once per
    /// application frame, after stepping.
    pub fn outgoing(&mut self, now_ms: u64) -> Vec<Outgoing> {
        self.handed = 0;
        self.check_caught_up();
        let mut out: Vec<Outgoing> = self
            .stream_out
            .drain(..)
            .map(|bytes| Outgoing {
                lane: Lane::Stream,
                bytes,
            })
            .collect();
        self.stream_bytes_sent += out.iter().map(|o| o.bytes.len() as u64).sum::<u64>();
        // A stamped input is sent until the host has built its tick, and at least once: a late
        // one is the host's only lateness sample.
        let newest = self.stamped.keys().next_back().copied();
        let unsent_from = self.sent_through.map_or(0, |t| t + 1);
        let keep_from = self.host_next_build.min(unsent_from);
        while self
            .stamped
            .first_key_value()
            .is_some_and(|(tick, _)| *tick < keep_from)
        {
            self.stamped.pop_first();
        }
        let mut first_tick = self
            .stamped
            .keys()
            .next()
            .copied()
            .unwrap_or(self.host_next_build);
        if let Some(newest) = newest {
            let span = newest.saturating_sub(first_tick) + 1;
            if span > MAX_INPUTS_PER_DATAGRAM as u64 {
                first_tick = newest + 1 - MAX_INPUTS_PER_DATAGRAM as u64;
            }
        }
        let inputs: Vec<Option<Vec<u8>>> = match newest {
            Some(newest) if newest >= first_tick => (first_tick..=newest)
                .map(|tick| self.stamped.get(&tick).cloned())
                .collect(),
            _ => Vec::new(),
        };
        if let Some(newest) = newest {
            self.sent_through = Some(self.sent_through.map_or(newest, |t| t.max(newest)));
        }
        let msg = InputsMsg {
            seq: self.out_seq,
            ack_next: self.next_expected,
            down_p99_run: self.down.p99_run(now_ms),
            down_window_run: self.down.window_run(now_ms),
            lead: self.extra_lead,
            hole: self.hole,
            leave: self.leave_requested,
            catching_up: self.catching_up,
            votes: self.votes.iter().take(MAX_VOTES_PER_DATAGRAM).copied().collect(),
            first_tick,
            inputs,
        };
        self.out_seq += 1;
        let bytes = wire::encode_inputs(&msg);
        self.datagrams_sent += 1;
        self.datagram_bytes_sent += bytes.len() as u64;
        out.push(Outgoing {
            lane: Lane::Datagram,
            bytes,
        });
        out
    }

    /// The next thing the application has to know or do.
    pub fn poll_event(&mut self) -> Option<SeatEvent> {
        self.events.pop_front()
    }

    /// What `net.lockstep` reports for a seat.
    pub fn status(&self) -> LockstepStatus {
        let (delay, delay_from) = self.stamper.current();
        LockstepStatus {
            role: "seat".to_owned(),
            tick: self.next_step,
            delay_ticks: delay,
            delay_from,
            gap_ticks: self.stamper.gap_ticks(),
            datagrams_sent: self.datagrams_sent,
            datagram_bytes_sent: self.datagram_bytes_sent,
            stream_bytes_sent: self.stream_bytes_sent,
            fp_env_corrections: self.fp_env_corrections,
            frames_repaired: self.frames_repaired,
            repair_ticks_max: self.repair_ticks_max,
            datagrams_lost: self.down.lost(),
            single_losses: self.single_losses,
            single_loss_repair_ticks_max: self.single_loss_repair_ticks_max,
            match_end_tick: self.ended_at,
            catchups: self.catchups,
            member: self.member,
            catching_up: self.catching_up,
            rollback_window_ticks: self.config.mode.window_ticks(),
            rollbacks: self.rollback.rollbacks,
            rollback_ticks_max: self.rollback.rollback_ticks_max,
            rollback_ticks_replayed: self.rollback.rollback_ticks_replayed,
            ticks_predicted: self.rollback.ticks_predicted,
            predictions_confirmed: self.rollback.predictions_confirmed,
            window_stalls: self.rollback.window_stalls,
            ahead_ticks: self.rollback.ahead_ticks,
            local_input_lag_ticks: self.rollback.local_input_lag_ticks,
            clock_lead_ticks: self.rollback.clock_lead_ticks,
            ..LockstepStatus::default()
        }
    }

    /// The game's rules for missing inputs, when set.
    pub(crate) fn input_rules(&self) -> Option<InputRules<G::Input>> {
        self.rules
    }

    /// Whether no capture maps to `tick` (a gap tick of a delay raise or of the start), as far as
    /// this seat's schedule knows.
    pub(crate) fn is_gap_tick(&self, tick: u64) -> bool {
        self.stamper.is_gap(tick)
    }

    /// The next capture tick the schedule stamps.
    pub(crate) fn next_capture(&self) -> u64 {
        self.stamper.next_capture()
    }

    /// Keeps the bodies of frames just applied in the migration cache, rolling the cache's base
    /// past the ones that fall out of it.
    fn cache_bodies(&mut self, bodies: Vec<Vec<u8>>) {
        if self.cache.is_empty() {
            // The cache starts where the frames it keeps start.
            self.cache_front = self.next_expected - bodies.len() as u64;
        }
        self.cache.extend(bodies);
        let max_input = self.config.max_input_bytes;
        while self.cache.len() > self.config.frame_log_ticks {
            let Some(old) = self.cache.pop_front() else {
                break;
            };
            if wire::decode_frame_body(&old, self.cache_front, &mut self.cache_base, max_input).is_err() {
                // Cannot happen for a body that decoded once; start the cache again from here.
                self.cache.clear();
                self.cache_base = self.state.clone();
                self.cache_front = self.next_expected;
                return;
            }
            self.cache_front += 1;
        }
    }

    /// The transport heard the host at `now_ms` (an authentic keepalive or acknowledgement): host
    /// migration judges the host's silence by this and by the lockstep payloads that arrive
    /// (spec 120 § 10.5: the host is lost once it has been interrupted for `migrate_after_ms`).
    pub fn note_host_heard(&mut self, now_ms: u64) {
        self.host_heard_ms = Some(self.host_heard_ms.map_or(now_ms, |at| at.max(now_ms)));
    }

    /// Milliseconds since anything of the host last arrived; `None` before the first payload.
    pub fn host_silent_ms(&self, now_ms: u64) -> Option<u64> {
        self.host_heard_ms.map(|at| now_ms.saturating_sub(at))
    }

    /// The migration roster the host shared ([`LockstepHost::share_roster`](super::LockstepHost::share_roster)),
    /// with its revision (bumped on each one), for a [`HostMigration`](super::HostMigration).
    pub fn migration_info(&self) -> Option<(u64, &MigrationRoster)> {
        self.roster.as_ref().map(|(rev, roster)| (*rev, roster))
    }

    /// The newest round-trip matrix the host shared.
    pub fn rtt_matrix(&self) -> Option<&RttMatrix> {
        self.matrix.as_ref()
    }

    /// The first tick the migration cache holds and how many frames it holds.
    pub fn frame_cache(&self) -> (u64, usize) {
        (self.cache_front, self.cache.len())
    }

    /// The cached bodies of the frames `from..next_expected`; `None` when the cache does not
    /// reach back to `from`.
    pub(crate) fn cached_bodies(&self, from: u64) -> Option<Vec<Vec<u8>>> {
        let skip = from.checked_sub(self.cache_front)?;
        if from > self.next_expected {
            return None;
        }
        Some(self.cache.iter().skip(skip as usize).cloned().collect())
    }

    /// The cache's base, its first tick and every body it holds.
    pub(crate) fn cache_parts(&self) -> (FrameState, u64, Vec<Vec<u8>>) {
        (
            self.cache_base.clone(),
            self.cache_front,
            self.cache.iter().cloned().collect(),
        )
    }

    /// The seat's stamp schedule, for a host that resumes the match from this seat.
    pub(crate) fn stamper_clone(&self) -> Stamper {
        self.stamper.clone()
    }

    /// The match moved to a new host (spec 120 § 10.5): what belonged to the old connection
    /// (its datagram sequence, the frames-hole report, which inputs were sent) starts again,
    /// the frame chain and the stamped inputs stay. Every stamped input the new host has not
    /// built yet is sent to it again.
    pub fn rehost(&mut self, now_ms: u64) {
        self.down = LossRuns::with_interval_us(self.config.tick_us());
        self.sent_through = None;
        self.hole = false;
        self.single_loss_from = None;
        self.host_heard_ms = Some(now_ms);
    }

    /// Frames this seat got from the stream, and frames datagrams it could not use yet because
    /// they started past the frames it has (the stream fills the gap).
    pub fn stream_stats(&self) -> (u64, u64) {
        (self.stream_frames, self.undecodable)
    }
}
