// SPDX-License-Identifier: MPL-2.0
//! The lockstep host: one frame per tick, sent to every seat with redundancy (spec 120 § 7.1-7.4).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::BuildHasher;

use super::delay::{DelayControl, SeatTiming};
use super::loss::{MAX_RUN, SeqClass};
use super::migration::{self, MigrationRoster, RTT_SHARE_MS, Resume, RttMatrix};
use super::quorum::{DesyncVerdict, Quorum, QuorumAction, VerdictKind, Voter};
use super::stamp::{Stamp, Stamper};
use super::watch::StampWatch;
use super::wire::{
    self, CatchupHead, FrameState, FramesHeader, KIND_INPUTS, KIND_PROBE_ROWS, KIND_REVOTE,
    KIND_SUBSYSTEM_ROWS, MAX_FRAMES_PER_PAYLOAD, MAX_MEMBERSHIP_PER_FRAME, MAX_SEATS, Order, RawFrame,
    SeatStream,
};
use super::{
    DelayPolicy, FrameRedundancy, HostInput, InputRules, Lane, LockstepConfig, LockstepError, LockstepFrame,
    LockstepStatus, MembershipChange, MembershipKind, MissingInputPolicy, Outgoing, SeatId, SeatInput,
    SeatLockstepStatus,
};
use crate::desync::{DesyncHash, DesyncReport, ProbeRows, SubsystemProbe, SubsystemRing};
use crate::feedback::{InputArrivals, LossMeter};
use crate::payload::{NetCodec, NetGame};
use crate::session::PeerId;
use crate::tick::TickDriver;

/// How often a seat gets its input timing record, milliseconds.
const FEEDBACK_INTERVAL_MS: u64 = 250;
/// Per-tick hashes the host keeps of its own simulation, for the probe.
pub(crate) const HASH_RING_TICKS: usize = 240;

/// Puts `tick`'s hash into a probe ring of `(tick, hash)` pairs, ascending by tick. A tick the
/// ring already holds gets the new hash: a replay or a state transfer stepped it again, and the
/// probe must compare the history the simulation has now, not the one it left (ADR-10147). A new
/// tick goes in tick order; once the ring is full the oldest tick is dropped (a tick older than
/// every kept one is not stored).
pub(crate) fn put_ring_hash(ring: &mut VecDeque<(u64, u64)>, tick: u64, hash: u64) {
    // The live path: each tick after the newest.
    if ring.back().is_none_or(|(newest, _)| *newest < tick) {
        if ring.len() >= HASH_RING_TICKS {
            ring.pop_front();
        }
        ring.push_back((tick, hash));
        return;
    }
    match ring.binary_search_by_key(&tick, |(t, _)| *t) {
        Ok(i) => ring[i].1 = hash,
        Err(i) if ring.len() < HASH_RING_TICKS => ring.insert(i, (tick, hash)),
        Err(0) => {}
        Err(i) => {
            ring.pop_front();
            ring.insert(i - 1, (tick, hash));
        }
    }
}
/// Bytes of frames one stream payload carries at most.
const STREAM_BATCH_BYTES: usize = 8 * 1024;
/// How long a seat that left still gets the frame that says so, ticks.
const LEFT_LINGER_TICKS: u64 = 600;
/// Missed ticks per seat remembered for a late merge, at most (about 4 s at 60 Hz): an input
/// older than that is not merged.
const MAX_MISSED_PER_SEAT: usize = 256;
/// Late inputs of one seat waiting to be merged into the next frame built, at most (one per
/// missed tick).
const MAX_LATE_PER_SEAT: usize = MAX_MISSED_PER_SEAT;
/// Arrivals in the feedback window before a rollback seat's median margin can release the hold
/// for it (half a second of inputs at 60 Hz).
const HOLD_RELEASE_SAMPLES: usize = 30;
/// Seats removed for silence remembered for a resume, at most.
const MAX_RETIRED: usize = 4096;

/// What the host process is, besides the frame authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostRole {
    /// A listen server: the host is also this seat's player. Its simulation votes as that seat.
    Listen(SeatId),
    /// A dedicated process that steps its own simulation as an observer; it has a vote.
    Observer,
    /// A dedicated process without a simulation: it builds frames and tallies the seats' votes.
    Relay,
}

impl HostRole {
    fn voter(self) -> Option<Voter> {
        match self {
            Self::Listen(seat) => Some(Voter::Seat(seat)),
            Self::Observer => Some(Voter::Observer),
            Self::Relay => None,
        }
    }

    fn local_seat(self) -> Option<SeatId> {
        match self {
            Self::Listen(seat) => Some(seat),
            _ => None,
        }
    }
}

/// Something the host application has to know or do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// The input delay changes: the frame at `tick` carries the new epoch.
    DelayChanged {
        /// The frame that carries the change.
        tick: u64,
        /// The new delay, ticks.
        delay: u16,
        /// First capture tick it applies to.
        delay_from: u64,
        /// Raised (`true`) or lowered.
        raised: bool,
    },
    /// A checkpoint's votes differed; the verdict says how it ended.
    Desync(DesyncVerdict),
    /// The host's own simulation is asked to replay from `from` (the last agreed checkpoint;
    /// `None`: the start) through `tick` and vote again with [`LockstepHost::revote`].
    SelfCheck {
        /// The agreed checkpoint to replay from.
        from: Option<u64>,
        /// The checkpoint to vote on again.
        tick: u64,
    },
    /// `seat` must take `source`'s state at `tick` (the transfer itself is the late-join path's).
    /// When `source` is the host's own voter, the host sends its state.
    ResyncSeat {
        /// The seat whose state differs.
        seat: SeatId,
        /// Whose state it takes.
        source: Voter,
        /// The checkpoint tick of the state.
        tick: u64,
        /// Counted transfers ordered for the seat so far.
        resyncs: u32,
        /// `false` for a tie: not held against the seat.
        counted: bool,
    },
    /// The host's own simulation must take `source`'s state at `tick`: the host was outvoted. It
    /// stays the frame authority; no seat is affected.
    ResyncHost {
        /// The majority seat that supplies the state.
        source: Voter,
        /// The checkpoint tick of the state.
        tick: u64,
        /// Counted transfers ordered for the host so far.
        resyncs: u32,
        /// `false` for a tie.
        counted: bool,
    },
    /// A seat leaves at the next frame built: it passed `max_resyncs` (`desync`, `tick` is the
    /// checkpoint that decided it), was silent past the timeout (`timeout`), or asked to leave
    /// (`left`); for the last two `tick` is the frame that lists the leave.
    SeatRemoved {
        /// The seat.
        seat: SeatId,
        /// The checkpoint that decided it, or the frame of the leave.
        tick: u64,
        /// The reason code: `desync`, `timeout` or `left`.
        reason: &'static str,
    },
    /// A seat has been silent for the policy's `interrupt_ms` (once per outage). It stays a
    /// member; its ticks are listed absent or held.
    SeatInterrupted {
        /// The seat.
        seat: SeatId,
    },
    /// An interrupted seat was heard again.
    SeatRestored {
        /// The seat.
        seat: SeatId,
    },
    /// A seat that caught up joins at `tick` (agreed-tick membership).
    SeatJoined {
        /// The seat.
        seat: SeatId,
        /// The frame that lists the join.
        tick: u64,
    },
    /// A seat that took a catch-up has stepped to the live edge: from `tick` on its inputs count.
    SeatCaughtUp {
        /// The seat.
        seat: SeatId,
        /// The next frame the host builds.
        tick: u64,
    },
    /// The match ended with the frame at `tick` ([`LockstepHost::end_match`]).
    MatchEnded {
        /// The last frame.
        tick: u64,
    },
    /// The host's own simulation passed `max_resyncs`: the host is at fault. The session should
    /// move to a majority peer (host migration decides how); no seat is removed.
    HostFault {
        /// The checkpoint that decided it.
        tick: u64,
        /// Transfers ordered for the host.
        resyncs: u32,
    },
    /// The votes cannot be decided: the match ends with `desync_unresolved`, without blame.
    Unresolved {
        /// The checkpoint that could not be decided.
        tick: u64,
    },
    /// The probe's second stage compared a diverged voter's subsystem rows at the first diverged
    /// tick with the reference's: it names the first diverged subsystem and the last agreed tick
    /// (spec 120 § 7.6, ADR-8211). Needs the simulations to hash with `record_state`.
    DesyncProbed(DesyncReport),
    /// The probe found where a diverged voter's hashes first differ from the reference's.
    DesyncLocated {
        /// The checkpoint of the verdict.
        tick: u64,
        /// The diverged voter.
        voter: Voter,
        /// The first tick its hash differs at.
        first_diverged_tick: u64,
    },
    /// A seat fell further behind than the frame log holds; it needs a state transfer to return.
    SeatLagging {
        /// The seat.
        seat: SeatId,
    },
    /// The verdict on a seat's input timing changed (ADR-8209): `suspect` when nine in ten of
    /// its inputs over the last ten seconds reached the host `hold_us` or more after the seat
    /// had acknowledged the frame that opened their capture, and its median margin before the
    /// deadline is no larger than that hold. Such a seat samples its input later than its capture
    /// tick. It is a flag for the application and its operators: the host removes nobody for it.
    LateStamp {
        /// The seat.
        seat: SeatId,
        /// Suspect now, or no longer.
        suspect: bool,
        /// The hold nine in ten of its inputs reached, microseconds.
        hold_us: u64,
        /// Its median margin before the frame deadline, microseconds.
        margin_us: i64,
    },
    /// `seat` was added again before its previous holder acknowledged the frame that lists its
    /// leave (ADR-8209). From now on [`LockstepHost::outgoing`] addresses the new holder under
    /// this seat; `payloads` are stream payloads with the frames the previous holder still lacks,
    /// through `leave_tick`. Send them to the connection that held the seat until now.
    SeatHandover {
        /// The seat that changed hands.
        seat: SeatId,
        /// The frame that lists the previous holder's leave: the last one it gets.
        leave_tick: u64,
        /// What to send to the previous holder, in order, on the stream.
        payloads: Vec<Outgoing>,
    },
}

/// An input as it arrived: the seat's bytes and what they decode to.
#[derive(Debug, Clone)]
struct Held<I> {
    bytes: Vec<u8>,
    value: I,
}

struct Logged<I> {
    tick: u64,
    body: Vec<u8>,
    frame: LockstepFrame<I>,
}

struct HostSeat<I> {
    timing: SeatTiming,
    arrivals: InputArrivals,
    /// Under rollback: the seat's median input arrives later than the rollback window, so the
    /// host no longer holds due frames for it (KI-NT-038, ADR-8855).
    hold_released: bool,
    /// The seat's upstream loss for its feedback, from packet-number progress (ADR-8077; the
    /// lockstep host has no quality tracker, ADR-8889).
    loss: LossMeter,
    /// The next frame tick the seat needs.
    acked_next: u64,
    /// The frames `stream_base..stream_next` were handed to the stream (the newest such run).
    stream_base: u64,
    stream_next: u64,
    out_seq: u64,
    lead: u16,
    missing: u64,
    too_far: u64,
    lag_max: u64,
    stream_frames: u64,
    lagging: bool,
    feedback_at_ms: Option<u64>,
    vote_ack: Option<u64>,
    /// The last frame the seat gets (the one that lists its leave).
    left_at: Option<u64>,
    orders: Vec<Order>,
    /// The loss run the seat asks its redundancy window to cover.
    window_run: u32,
    /// The seat reported a hole: every unacknowledged frame is sent until it acknowledges this tick.
    repair_to: Option<u64>,
    hole_repairs: u64,
    /// The first frame never handed to a datagram or the stream for this seat.
    sent_next: u64,
    /// When the seat was admitted (the first frame built after it was added or caught up).
    admitted_ms: Option<u64>,
    /// When anything of the seat last arrived.
    last_heard_ms: Option<u64>,
    /// The seat has been heard since its admission (it is past loading).
    heard_any: bool,
    interrupted: bool,
    /// The seat asked to leave, or was removed for silence: no second removal.
    leaving: bool,
    /// The seat took a catch-up and is not a member yet: it joins once it has stepped to the edge.
    joining: bool,
    /// The seat is stepping a catch-up's frames.
    catching_up: bool,
    /// The catch-up's frames end before this tick: the seat is clear once it acknowledges it.
    catchup_through: u64,
    /// A catch-up was sent and no datagram of the seat has said it holds it yet: until one does,
    /// the seat's acknowledgements, holes and leave requests are about its old chain.
    awaiting_catchup: bool,
    /// The first datagram sequence of the seat's new chain; earlier datagrams are the old one's.
    catchup_seq: Option<u64>,
    /// Ticks before this are not counted missing (the start or the join of the seat).
    count_missing_from: u64,
    /// Ticks built without the seat's input, for a late merge.
    missed: BTreeSet<u64>,
    /// Late inputs waiting for the next frame built.
    late: Vec<(u64, Held<I>)>,
    late_merged: u64,
    held: u64,
    /// Catch-up payloads for the stream.
    stream_out: Vec<Vec<u8>>,
    /// Its input timing against its own acknowledgements (late stamping).
    watch: StampWatch,
    /// The transport's round trip to the seat, milliseconds (the migration matrix).
    rtt_ms: Option<u64>,
}

impl<I> HostSeat<I> {
    fn new(first_frame: u64, tick_us: u64) -> Self {
        Self {
            timing: SeatTiming::new(tick_us),
            arrivals: InputArrivals::default(),
            hold_released: false,
            loss: LossMeter::default(),
            acked_next: first_frame,
            stream_base: first_frame,
            stream_next: first_frame,
            out_seq: 0,
            lead: 0,
            missing: 0,
            too_far: 0,
            lag_max: 0,
            stream_frames: 0,
            lagging: false,
            feedback_at_ms: None,
            vote_ack: None,
            left_at: None,
            orders: Vec::new(),
            window_run: 0,
            repair_to: None,
            hole_repairs: 0,
            sent_next: first_frame,
            admitted_ms: None,
            last_heard_ms: None,
            heard_any: false,
            interrupted: false,
            leaving: false,
            joining: false,
            catching_up: false,
            catchup_through: 0,
            awaiting_catchup: false,
            catchup_seq: None,
            count_missing_from: first_frame,
            missed: BTreeSet::new(),
            late: Vec::new(),
            late_merged: 0,
            held: 0,
            stream_out: Vec::new(),
            watch: StampWatch::default(),
            rtt_ms: None,
        }
    }

    fn silent_ms(&self, now_ms: u64) -> u64 {
        self.last_heard_ms
            .or(self.admitted_ms)
            .map_or(0, |at| now_ms.saturating_sub(at))
    }

    /// The first frame from `tick` on that is not already on the stream.
    fn after_stream(&self, tick: u64) -> u64 {
        if (self.stream_base..self.stream_next).contains(&tick) {
            self.stream_next
        } else {
            tick
        }
    }
}

struct ProbeJob {
    tick: u64,
    from: u64,
    reference: Voter,
    rows: BTreeMap<Voter, Vec<u64>>,
    /// The diverged voters probed (at most `SubsystemProbe::MAX_TARGETS`), and those located.
    diverged: Vec<Voter>,
    located: BTreeSet<Voter>,
}

/// The frame authority of a lockstep match (spec 120 § 7.1).
///
/// Drive it once per host tick: [`Self::receive`] what the seats sent, [`Self::poll_frame`] until
/// it returns `None` (step the host's own simulation from each frame, if it has one), then send
/// everything [`Self::outgoing`] returns.
pub struct LockstepHost<G: NetGame> {
    config: LockstepConfig,
    role: HostRole,
    tick_us: u64,
    origin_us: Option<u64>,
    /// The tick due at `origin_us`: 0, or the tick a migrated match resumed at (spec 120 § 10.5).
    tick_base: u64,
    driver: TickDriver<Held<G::Input>>,
    members: BTreeSet<SeatId>,
    pending: Vec<MembershipChange>,
    /// Seats added again while their leave was pending: they join on the frame after it.
    rejoin: BTreeSet<SeatId>,
    seats: BTreeMap<SeatId, HostSeat<G::Input>>,
    stamper: Stamper,
    open_capture: Option<Stamp>,
    local_used: bool,
    control: DelayControl,
    state: FrameState,
    log: VecDeque<Logged<G::Input>>,
    next_tick: u64,
    quorum: Quorum,
    hash_ring: VecDeque<(u64, u64)>,
    events: VecDeque<HostEvent>,
    probe: Option<ProbeJob>,
    /// Subsystem rows of the host's own simulation (spec 120 § 7.6).
    rows: SubsystemRing,
    /// The host's rows of the last probe window, kept as they were when the probe started.
    probe_hold: Option<ProbeRows>,
    subsystem_probe: Option<SubsystemProbe>,
    desync_reports: u64,
    last_desync: Option<DesyncReport>,
    disturbed: bool,
    last_check_tick: u64,
    datagrams_sent: u64,
    datagram_bytes_sent: u64,
    stream_bytes_sent: u64,
    fp_env_corrections: u64,
    rejected: u64,
    rules: Option<InputRules<G::Input>>,
    /// The delta base after the tick before the log's first frame (a catch-up starts from it).
    log_base: FrameState,
    end_pending: bool,
    ended_at: Option<u64>,
    /// Seats removed for silence, and when: they may come back with their resume token.
    retired: BTreeMap<SeatId, u64>,
    /// Keys the resume tokens (`token_of`); a per-host secret, never part of a frame or the sim state.
    // det-scan: allow(os_random): resume-token key only; tokens authenticate a seat and never enter a frame or a hash
    token_key: std::hash::RandomState,
    /// Bumped each time the game gives a seat id to a new player: an old token stops working.
    generations: BTreeMap<SeatId, u32>,
    /// Seats `resume` named whose catch-up has not been sent yet.
    resumed: BTreeSet<SeatId>,
    /// A frame of the log did not decode against the base: no catch-up can be built any more.
    log_base_broken: bool,
    seats_timed_out: u64,
    seats_left: u64,
    catchups: u64,
    /// The migration secret of the match: it keys the resume tokens when set (spec 120 § 10.5).
    match_secret: Option<[u8; 32]>,
    /// The migration roster shared with the seats, and whether it has to be shared again.
    roster: Option<MigrationRoster>,
    roster_dirty: bool,
    /// The round-trip matrix's last version, the tick it was shared at, and whether the seats
    /// changed since.
    rtt_version: u32,
    rtt_shared_tick: Option<u64>,
    matrix_dirty: bool,
    /// Round trips between seats the game measured, milliseconds.
    pair_rtt: BTreeMap<(SeatId, SeatId), u16>,
}

impl<G: NetGame> std::fmt::Debug for LockstepHost<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockstepHost")
            .field("role", &self.role)
            .field("next_tick", &self.next_tick)
            .field("members", &self.members)
            .finish_non_exhaustive()
    }
}

fn peer(seat: SeatId) -> PeerId {
    PeerId(u32::from(seat.0))
}

/// The encoded body of `tick` in a log whose first frame is `front` (ticks are contiguous).
fn body_at<I>(log: &VecDeque<Logged<I>>, front: u64, tick: u64) -> &[u8] {
    tick.checked_sub(front)
        .and_then(|i| log.get(i as usize))
        .map_or(&[][..], |l| l.body.as_slice())
}

/// Frames a frames datagram carries at most under [`FrameRedundancy::Interleaved`] outside a
/// repair, the newest included: a loss run longer than this is repaired through the hole report.
const MAX_WINDOW_FRAMES: u64 = 16;
/// Under [`FrameRedundancy::Interleaved`] every this many frames datagrams reach back over the
/// link's loss run; the others carry the newest frame and the one before it.
const DEEP_EVERY: u64 = 2;
/// How many frames the datagram with per-seat counter `phase` carries, the newest included, for a
/// link whose redundancy window has to cover a loss run of `run` datagrams.
///
/// A run of `L` lost datagrams is repaired by the first datagram `j` ticks after it that carries
/// at least `L + j` frames (the seat applies frames in order, so the datagram has to reach back
/// to the first one it lacks).
///
/// * [`FrameRedundancy::Adaptive`]: `run + 1` frames in every datagram (two at least): any run up
///   to `run` is repaired by the next datagram.
/// * [`FrameRedundancy::Interleaved`]: two frames in every datagram, so a single loss is still
///   repaired by the next datagram; every [`DEEP_EVERY`]-th datagram carries `run + DEEP_EVERY`
///   frames, so any run up to `run` is repaired within [`DEEP_EVERY`] ticks. A link whose runs are
///   single losses (`run <= 1`) never needs the deeper datagram.
fn window_depth(redundancy: FrameRedundancy, run: u32, phase: u64) -> u64 {
    let run = u64::from(run);
    match redundancy {
        FrameRedundancy::AllUnacked => u64::MAX,
        FrameRedundancy::Adaptive => (run + 1).max(2),
        FrameRedundancy::Interleaved if run >= 2 && phase.is_multiple_of(DEEP_EVERY) => {
            (run + DEEP_EVERY).min(MAX_WINDOW_FRAMES)
        }
        FrameRedundancy::Interleaved => 2,
    }
}

/// The frames `start..end` of `log` as stream payloads, oldest first: `(frames, bytes)` per
/// payload, each at most [`STREAM_BATCH_BYTES`] of bodies and [`MAX_FRAMES_PER_PAYLOAD`] frames.
fn stream_batches<I>(
    log: &VecDeque<Logged<I>>,
    front: u64,
    mut start: u64,
    end: u64,
) -> Vec<(usize, Vec<u8>)> {
    let body_of = |tick: u64| body_at(log, front, tick);
    let mut out = Vec::new();
    while start < end {
        let mut batch_end = start;
        let mut batch_bytes = 0usize;
        while batch_end < end
            && ((batch_end - start) as usize) < MAX_FRAMES_PER_PAYLOAD
            && (batch_end == start || batch_bytes + body_of(batch_end).len() <= STREAM_BATCH_BYTES)
        {
            batch_bytes += body_of(batch_end).len();
            batch_end += 1;
        }
        let count = (batch_end - start) as usize;
        out.push((
            count,
            wire::encode_stream_frames(start, count, (start..batch_end).map(body_of)),
        ));
        start = batch_end;
    }
    out
}

impl<G: NetGame> LockstepHost<G> {
    /// A host with `config` (sanitized) in `role`. Add the seats, then [`Self::start`].
    pub fn new(config: LockstepConfig, role: HostRole) -> Self {
        let config = config.sanitized();
        let quorum = Quorum::new(&config, role.voter());
        Self {
            tick_us: config.tick_us(),
            origin_us: None,
            tick_base: 0,
            driver: TickDriver::new(config.input_buffer_ticks),
            members: BTreeSet::new(),
            pending: Vec::new(),
            rejoin: BTreeSet::new(),
            seats: BTreeMap::new(),
            stamper: Stamper::new(config.initial_delay_ticks),
            open_capture: None,
            local_used: false,
            control: DelayControl::new(
                config.delay_policy,
                config.delay_window_ms,
                config.calm_windows_to_lower,
                config.min_delay_ticks,
                config.max_delay_ticks,
            ),
            state: FrameState::new(config.initial_delay_ticks),
            log: VecDeque::new(),
            next_tick: 0,
            quorum,
            hash_ring: VecDeque::with_capacity(HASH_RING_TICKS),
            events: VecDeque::new(),
            probe: None,
            rows: SubsystemRing::new(),
            probe_hold: None,
            subsystem_probe: None,
            desync_reports: 0,
            last_desync: None,
            disturbed: false,
            last_check_tick: 0,
            datagrams_sent: 0,
            datagram_bytes_sent: 0,
            stream_bytes_sent: 0,
            fp_env_corrections: 0,
            rejected: 0,
            rules: None,
            log_base: FrameState::new(config.initial_delay_ticks),
            end_pending: false,
            ended_at: None,
            retired: BTreeMap::new(),
            // det-scan: allow(os_random): resume-token key only; tokens authenticate a seat and never enter a frame or a hash
            token_key: std::hash::RandomState::new(),
            generations: BTreeMap::new(),
            resumed: BTreeSet::new(),
            log_base_broken: false,
            seats_timed_out: 0,
            seats_left: 0,
            catchups: 0,
            match_secret: None,
            roster: None,
            roster_dirty: false,
            rtt_version: 0,
            rtt_shared_tick: None,
            matrix_dirty: false,
            pair_rtt: BTreeMap::new(),
            role,
            config,
        }
    }

    /// A relay host that continues a migrated match (spec 120 § 10.5): the frames of the winner's
    /// cache plus the ones the survivors sent, the next frame at the highest next tick, due at
    /// `resume_at_ms`, every member of the chain a seat (the ones that did not say hello wait
    /// silent and are judged by the timeout policy like any seat).
    pub(crate) fn resumed(config: LockstepConfig, r: Resume) -> Result<Self, LockstepError> {
        let mut host = Self::new(config, HostRole::Relay);
        let max_input = host.config.max_input_bytes;
        let mut state = r.log_base.clone();
        let mut stamper = r.stamper;
        // The winner's schedule already holds the epochs of the frames it had.
        let own_next = r.seats_next.get(&r.winner).copied().unwrap_or(r.front);
        for (i, body) in r.bodies.into_iter().enumerate() {
            let tick = r.front + i as u64;
            let raw = wire::decode_frame_body(&body, tick, &mut state, max_input)?;
            if let Some((delay, from)) = raw.delay_change
                && tick >= own_next
            {
                stamper.set_delay(from, delay);
            }
            let frame = frame_of::<G>(&raw, &state, &host.config, None)?;
            host.log.push_back(Logged { tick, body, frame });
        }
        let next = r.front + host.log.len() as u64;
        while stamper.next_capture() < next {
            stamper.stamp_next(0);
        }
        host.open_capture = (stamper.next_capture() == next).then(|| stamper.stamp_next(0).1);
        host.stamper = stamper;
        host.log_base = r.log_base;
        host.next_tick = next;
        host.tick_base = next;
        host.last_check_tick = next;
        host.origin_us = Some(r.resume_at_ms.saturating_mul(1000));
        host.driver.resume_at(next);
        host.members = state.seats.keys().copied().collect();
        host.state = state;
        host.match_secret = Some(r.roster.secret);
        for entry in &r.roster.seats {
            host.generations.insert(entry.seat, entry.generation);
        }
        let front = r.front;
        for seat in host.members.clone() {
            host.driver.add_peer(peer(seat));
            host.quorum.admit(Voter::Seat(seat));
            // A seat that said hello needs the frames from its next tick on; one that did not is
            // sent everything the log holds (it acknowledges what it has).
            let from = r
                .seats_next
                .get(&seat)
                .copied()
                .unwrap_or(front)
                .clamp(front, next);
            let mut hs = HostSeat::new(from, host.tick_us);
            hs.count_missing_from = next;
            hs.admitted_ms = Some(r.now_ms);
            if r.seats_next.contains_key(&seat) {
                hs.last_heard_ms = Some(r.now_ms);
                hs.heard_any = true;
            }
            hs.rtt_ms = r
                .matrix
                .host_rtt_ms
                .get(&seat)
                .copied()
                .flatten()
                .filter(|_| seat != r.winner)
                .map(u64::from);
            if seat == r.winner {
                hs.rtt_ms = Some(0);
            }
            host.seats.insert(seat, hs);
        }
        host.rtt_version = r.matrix.version;
        host.pair_rtt = r.matrix.pair_rtt_ms;
        host.roster = Some(r.roster);
        host.roster_dirty = true;
        host.matrix_dirty = true;
        Ok(host)
    }

    /// Shares the match's migration roster with every seat over the stream (spec 120 § 10.5):
    /// each seat's Noise static key and address and the per-match migration secret, so the seats
    /// can elect a new host when this one quits. Send the seats' lockstep stream over the sealed
    /// link only. From now on resume tokens are keyed by the migration secret (any host of the
    /// match can check them), the roster is shared again when a seat id gets a new player, and
    /// a round-trip matrix ([`Self::note_transport_rtt`], [`Self::note_pair_rtt`]) follows every
    /// three seconds and when the seats change. A seat id generation the host knows replaces the
    /// roster's.
    pub fn share_roster(&mut self, roster: MigrationRoster) {
        self.match_secret = Some(roster.secret);
        self.roster = Some(roster);
        self.roster_dirty = true;
        self.matrix_dirty = true;
    }

    /// The migration roster this host shares, when it shares one.
    pub fn migration_roster(&self) -> Option<&MigrationRoster> {
        self.roster.as_ref()
    }

    /// A round trip the game measured between two seats, milliseconds: the migration matrix uses
    /// it instead of the sum of both seats' round trips to the host.
    pub fn note_pair_rtt(&mut self, a: SeatId, b: SeatId, rtt_ms: u64) {
        if a != b {
            self.pair_rtt
                .insert((a.min(b), a.max(b)), rtt_ms.min(u64::from(u16::MAX - 1)) as u16);
        }
    }

    /// The seats a roster and a matrix name: every seat that has not left.
    fn live_seats(&self) -> impl Iterator<Item = (&SeatId, &HostSeat<G::Input>)> {
        self.seats
            .iter()
            .filter(|(_, hs)| hs.left_at.is_none() && !hs.leaving)
    }

    /// Queues the roster and the round-trip matrix to every seat when they are due.
    fn queue_migration_info(&mut self) {
        let Some(roster) = &self.roster else {
            return;
        };
        let local = self.role.local_seat();
        if std::mem::take(&mut self.roster_dirty) {
            let mut shared = roster.clone();
            let live: BTreeSet<SeatId> = self.live_seats().map(|(s, _)| *s).collect();
            shared.seats.retain(|s| live.contains(&s.seat));
            for entry in &mut shared.seats {
                if let Some(generation) = self.generations.get(&entry.seat) {
                    entry.generation = *generation;
                }
            }
            let bytes = migration::encode_roster(&shared);
            for (seat, hs) in &mut self.seats {
                if Some(*seat) != local && hs.left_at.is_none() {
                    hs.stream_out.push(bytes.clone());
                }
            }
            self.roster = Some(shared);
        }
        let every = (RTT_SHARE_MS * u64::from(self.config.tick_rate_hz))
            .div_ceil(1000)
            .max(1);
        let due = self
            .rtt_shared_tick
            .is_none_or(|at| self.next_tick >= at.saturating_add(every));
        if !(std::mem::take(&mut self.matrix_dirty) || due) {
            return;
        }
        self.rtt_version = self.rtt_version.wrapping_add(1);
        self.rtt_shared_tick = Some(self.next_tick);
        let matrix = RttMatrix {
            version: self.rtt_version,
            host_rtt_ms: self
                .live_seats()
                .map(|(seat, hs)| {
                    let rtt = if Some(*seat) == local { Some(0) } else { hs.rtt_ms };
                    (
                        *seat,
                        rtt.map(|ms| ms.min(u64::from(migration::RTT_MAX_MS)) as u16),
                    )
                })
                .collect(),
            pair_rtt_ms: self.pair_rtt.clone(),
            silent: self
                .live_seats()
                .filter(|(seat, hs)| Some(**seat) != local && (hs.interrupted || !hs.heard_any))
                .map(|(seat, _)| *seat)
                .collect(),
        };
        let bytes = migration::encode_rtt(&matrix);
        for (seat, hs) in &mut self.seats {
            if Some(*seat) != local && hs.left_at.is_none() {
                hs.stream_out.push(bytes.clone());
            }
        }
    }

    /// Sets the game's rules for missing and late inputs (spec 120 § 6.3, § 7.4). Every seat of
    /// the match sets the same rules with [`LockstepSeat::set_input_rules`](super::LockstepSeat::set_input_rules).
    pub fn set_input_rules(&mut self, rules: InputRules<G::Input>) {
        self.rules = Some(rules);
    }

    /// Ends the match: the next frame built is the last one and every seat steps it ([`LockstepFrame::match_end`]).
    pub fn end_match(&mut self) {
        if self.ended_at.is_none() {
            self.end_pending = true;
        }
    }

    /// The tick of the frame that ended the match.
    pub fn ended_at(&self) -> Option<u64> {
        self.ended_at
    }

    /// The token a seat presents to [`Self::resume`] after it lost its connection: the host
    /// application hands it to the seat's player at admission (spec 120 § 7.4). It is a keyed
    /// hash under a secret of this host, so a peer cannot forge another seat's token.
    pub fn resume_token(&self, seat: SeatId) -> Option<u64> {
        let known = self
            .seats
            .get(&seat)
            .is_some_and(|hs| hs.left_at.is_none() && !hs.leaving)
            || self.retired.contains_key(&seat);
        known.then(|| self.token_of(seat))
    }

    fn token_of(&self, seat: SeatId) -> u64 {
        let generation = self.generations.get(&seat).copied().unwrap_or(0);
        if let Some(secret) = &self.match_secret {
            return migration::resume_token(secret, seat, generation);
        }
        self.token_key
            .hash_one((b"regia.lockstep.resume", seat.0, generation))
    }

    /// A new player takes `seat`: tokens issued for the seat's earlier players stop working.
    fn new_occupant(&mut self, seat: SeatId) {
        *self.generations.entry(seat).or_default() += 1;
        self.roster_dirty = true;
        self.matrix_dirty = true;
        self.retired.remove(&seat);
        self.resumed.remove(&seat);
    }

    /// The seat a resume token names, when it may come back: it is still a seat of the match, or
    /// it was removed for silence within [`LockstepConfig::resume_window_ms`]. Follow with
    /// [`Self::catch_up`] for that seat; it keeps its seat id.
    pub fn resume(&mut self, token: u64, now_ms: u64) -> Result<SeatId, LockstepError> {
        let seat = self
            .resumable(now_ms)
            .into_iter()
            .find(|seat| self.token_of(*seat) == token)
            .ok_or(LockstepError::ResumeRefused)?;
        self.resumed.insert(seat);
        Ok(seat)
    }

    /// [`Self::resume`] for a seat the transport already identified (wire 6, ADR-10052 (2)): the
    /// seat's connection came back with the transport's own resume ticket (`NetServer`'s
    /// `ServerEvent::PeerResumed` for the peer that held it, [`SeatPeers`](super::net::SeatPeers)),
    /// so no token message has to cross the link first. The same rule as the token: the seat is
    /// still a member, or was removed for silence within [`LockstepConfig::resume_window_ms`];
    /// otherwise [`LockstepError::ResumeRefused`]. A member resumes in place; follow with
    /// [`Self::catch_up`] for a removed one (or a seat whose process lost its state).
    pub fn resume_seat(&mut self, seat: SeatId, now_ms: u64) -> Result<SeatId, LockstepError> {
        if !self.resumable(now_ms).contains(&seat) {
            return Err(LockstepError::ResumeRefused);
        }
        self.resumed.insert(seat);
        Ok(seat)
    }

    /// The seats that may come back now: the members, and the seats removed for silence within
    /// the resume window.
    fn resumable(&self, now_ms: u64) -> Vec<SeatId> {
        let window = self.config.resume_window_ms;
        self.seats
            .iter()
            .filter(|(_, hs)| hs.left_at.is_none() && !hs.leaving)
            .map(|(seat, _)| *seat)
            .chain(
                self.retired
                    .iter()
                    .filter(|(_, at)| now_ms.saturating_sub(**at) <= window)
                    .map(|(seat, _)| *seat),
            )
            .collect()
    }

    /// Sends `seat` a catch-up (spec 120 § 7.4): the game's serialized `state` after the host
    /// stepped `state_tick`, the frame chain's delta base after that tick, and every frame since,
    /// on the stream. The seat steps them under its per-frame budget; a seat that is not a member
    /// joins on an agreed tick once it has stepped to the live edge ([`HostEvent::SeatJoined`]),
    /// a member resumes in place. Returns the number of frames sent. Refused with
    /// [`LockstepError::CatchupGap`] when the frame log does not hold every frame after
    /// `state_tick`.
    pub fn catch_up(
        &mut self,
        seat: SeatId,
        state_tick: u64,
        state: &[u8],
        now_ms: u64,
    ) -> Result<u64, LockstepError> {
        if self.ended_at.is_some() || self.end_pending {
            return Err(LockstepError::MatchEnded);
        }
        if !self.started() {
            return Err(LockstepError::NotStarted);
        }
        let front = self.log.front().map_or(self.next_tick, |l| l.tick);
        let gap = LockstepError::CatchupGap {
            state_tick,
            first_logged: front,
            next_build: self.next_tick,
        };
        if state_tick >= self.next_tick || state_tick.saturating_add(1) < front {
            return Err(gap);
        }
        if state.len() > wire::MAX_STATE_BYTES {
            return Err(LockstepError::Malformed("state past the catch-up limit"));
        }
        if self.log_base_broken {
            return Err(LockstepError::Malformed("the frame log's base was lost"));
        }
        if self
            .pending
            .iter()
            .any(|c| c.seat == seat && c.kind == MembershipKind::Leave)
            || Some(seat) == self.role.local_seat()
        {
            return Err(LockstepError::UnknownSeat(seat));
        }
        // The delta base after `state_tick`: the log's base rolled through the frames up to it.
        let mut base = self.log_base.clone();
        for logged in self.log.iter().take_while(|l| l.tick <= state_tick) {
            wire::decode_frame_body(&logged.body, logged.tick, &mut base, self.config.max_input_bytes)
                .map_err(|_| LockstepError::Malformed("frame log does not decode"))?;
        }
        let member = self.members.contains(&seat)
            || self
                .pending
                .iter()
                .any(|c| c.seat == seat && c.kind == MembershipKind::Join);
        let next_tick = self.next_tick;
        // A seat id the game did not resume and that is not a member goes to a new player.
        if !member && !self.resumed.contains(&seat) {
            self.new_occupant(seat);
        }
        self.resumed.remove(&seat);
        // Per-connection state starts again (the seat may be a new process whose datagram
        // sequence starts at 0); the counters of the seat carry over.
        let mut hs = HostSeat::new(next_tick, self.tick_us);
        hs.joining = !member;
        if let Some(old) = self.seats.remove(&seat) {
            hs.out_seq = old.out_seq;
            hs.missing = old.missing;
            hs.too_far = old.too_far;
            hs.lag_max = old.lag_max;
            hs.stream_frames = old.stream_frames;
            hs.hole_repairs = old.hole_repairs;
            hs.late_merged = old.late_merged;
            hs.held = old.held;
            hs.count_missing_from = old.count_missing_from;
            if member {
                hs.orders = old.orders;
            }
        }
        self.seats.insert(seat, hs);
        self.retired.remove(&seat);
        self.roster_dirty = true;
        let mut payloads = vec![wire::encode_catchup(&CatchupHead {
            state_tick,
            base,
            state_len: state.len(),
        })];
        payloads.extend(state.chunks(STREAM_BATCH_BYTES).map(wire::encode_state_chunk));
        let log = &self.log;
        let body_of = |tick: u64| body_at(log, front, tick);
        let mut start = state_tick + 1;
        while start < next_tick {
            let mut end = start;
            let mut bytes = 0usize;
            while end < next_tick
                && ((end - start) as usize) < MAX_FRAMES_PER_PAYLOAD
                && (end == start || bytes + body_of(end).len() <= STREAM_BATCH_BYTES)
            {
                bytes += body_of(end).len();
                end += 1;
            }
            payloads.push(wire::encode_stream_frames(
                start,
                (end - start) as usize,
                (start..end).map(body_of),
            ));
            start = end;
        }
        let frames = next_tick - (state_tick + 1);
        let Some(hs) = self.seats.get_mut(&seat) else {
            return Err(LockstepError::UnknownSeat(seat));
        };
        hs.stream_out.extend(payloads);
        hs.stream_frames += frames;
        hs.acked_next = next_tick;
        hs.stream_base = state_tick + 1;
        hs.stream_next = next_tick;
        hs.sent_next = next_tick;
        hs.repair_to = None;
        hs.lagging = false;
        hs.catching_up = true;
        hs.catchup_through = next_tick;
        hs.awaiting_catchup = true;
        hs.catchup_seq = None;
        hs.admitted_ms = Some(now_ms);
        hs.last_heard_ms = None;
        hs.heard_any = false;
        hs.interrupted = false;
        hs.missed.clear();
        hs.late.clear();
        self.catchups += 1;
        Ok(frames)
    }

    /// The settings in force.
    pub fn config(&self) -> &LockstepConfig {
        &self.config
    }

    /// The host's role.
    pub fn role(&self) -> HostRole {
        self.role
    }

    /// The next tick to build.
    pub fn tick(&self) -> u64 {
        self.next_tick
    }

    /// The input delay in force for the next capture tick, ticks.
    pub fn delay_ticks(&self) -> u16 {
        self.stamper.current().0
    }

    /// The seats listed in the last frame built.
    pub fn members(&self) -> impl Iterator<Item = SeatId> + '_ {
        self.members.iter().copied()
    }

    /// Lists `seat` from the next frame built on (agreed-tick membership). A seat added before
    /// [`Self::start`] is listed from tick 0. Adding a member again changes nothing. A seat whose
    /// leave is still waiting for its frame joins on the frame after that one, as a new seat: a
    /// leave and a join of one seat are never in the same frame.
    ///
    /// Inputs for the seat are buffered from this call on, so an input that reaches the host
    /// before the frame that lists the join is in that frame. When the seat's previous holder
    /// has not acknowledged the frame that lists its leave, the frames it still lacks are handed
    /// out with [`HostEvent::SeatHandover`].
    pub fn add_seat(&mut self, seat: SeatId) {
        let listed = self.members.contains(&seat)
            || self
                .pending
                .iter()
                .any(|c| c.seat == seat && c.kind == MembershipKind::Join);
        if !listed {
            self.new_occupant(seat);
        }
        self.admit(seat);
    }

    /// Lists `seat` from the next frame built on, without giving the seat id to a new player.
    fn admit(&mut self, seat: SeatId) {
        let joining = self
            .pending
            .iter()
            .rev()
            .find(|c| c.seat == seat)
            .map(|c| c.kind == MembershipKind::Join);
        if joining == Some(false) {
            self.rejoin.insert(seat);
            return;
        }
        if joining.unwrap_or(self.members.contains(&seat)) {
            return;
        }
        let joins = self
            .pending
            .iter()
            .filter(|c| c.kind == MembershipKind::Join)
            .count();
        if self.members.len() + joins >= MAX_SEATS {
            // A frame lists at most this many seats; one more would not decode anywhere.
            tracing::warn!(%seat, "lockstep seat not added: the match has {MAX_SEATS} seats");
            return;
        }
        self.pending.push(MembershipChange {
            seat,
            kind: MembershipKind::Join,
        });
        self.hand_over(seat);
        self.seats
            .insert(seat, HostSeat::new(self.next_tick, self.tick_us));
        // Buffer its inputs now: the frame that lists the join takes what arrived before it.
        self.driver.add_peer(peer(seat));
        self.quorum.admit(Voter::Seat(seat));
    }

    /// The seat's row is about to become a new holder's. A previous holder that has not
    /// acknowledged the frame that lists its leave is still owed its orders and the frames
    /// through that one: they go to the application, on the stream (delivered once, in order,
    /// from the frame the holder acknowledged), since [`Self::outgoing`] addresses the new holder
    /// from here on.
    fn hand_over(&mut self, seat: SeatId) {
        if Some(seat) == self.role.local_seat() {
            return;
        }
        let Some(old) = self.seats.get_mut(&seat) else {
            return;
        };
        let Some(leave_tick) = old.left_at else {
            return;
        };
        let mut payloads: Vec<Outgoing> = old
            .orders
            .drain(..)
            .map(|order| Outgoing {
                lane: Lane::Stream,
                bytes: wire::encode_order(&order),
            })
            .collect();
        let start = old.after_stream(old.acked_next);
        if let Some(front) = self.log.front().map(|l| l.tick)
            && !old.lagging
            && (front..=leave_tick).contains(&start)
        {
            payloads.extend(
                stream_batches(&self.log, front, start, leave_tick + 1)
                    .into_iter()
                    .map(|(_, bytes)| Outgoing {
                        lane: Lane::Stream,
                        bytes,
                    }),
            );
        }
        if payloads.is_empty() {
            return;
        }
        self.stream_bytes_sent += payloads.iter().map(|p| p.bytes.len() as u64).sum::<u64>();
        self.events.push_back(HostEvent::SeatHandover {
            seat,
            leave_tick,
            payloads,
        });
    }

    /// Stops listing `seat` from the next frame built on; every seat sees the leave on that tick.
    pub fn remove_seat(&mut self, seat: SeatId) {
        if self.rejoin.remove(&seat) {
            // The join that was waiting for the leave's frame is dropped; the leave stands.
            return;
        }
        let joining = self
            .pending
            .iter()
            .rev()
            .find(|c| c.seat == seat)
            .map(|c| c.kind == MembershipKind::Join);
        if !joining.unwrap_or(self.members.contains(&seat)) {
            return;
        }
        self.pending.push(MembershipChange {
            seat,
            kind: MembershipKind::Leave,
        });
    }

    /// Starts the frame clock: tick 0 is due at `now_ms`.
    pub fn start(&mut self, now_ms: u64) {
        if self.origin_us.is_some() {
            return;
        }
        self.origin_us = Some(now_ms.saturating_mul(1000));
        self.open_capture = Some(self.stamper.stamp_next(0).1);
    }

    /// Whether [`Self::start`] ran.
    pub fn started(&self) -> bool {
        self.origin_us.is_some()
    }

    fn due_us(&self, tick: u64) -> Option<u64> {
        Some(
            self.origin_us?
                .saturating_add(tick.saturating_sub(self.tick_base).saturating_mul(self.tick_us)),
        )
    }

    /// Takes one lockstep payload a seat sent on `lane`.
    pub fn receive(
        &mut self,
        seat: SeatId,
        lane: Lane,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<(), LockstepError> {
        if !self.seats.contains_key(&seat) {
            return Err(LockstepError::UnknownSeat(seat));
        }
        if let Some(hs) = self.seats.get_mut(&seat) {
            hs.last_heard_ms = Some(now_ms);
            // A seat heard for the first time, or again, is a candidate of the migration matrix.
            self.matrix_dirty |= !hs.heard_any || hs.interrupted;
            hs.heard_any = true;
            if hs.interrupted {
                hs.interrupted = false;
                self.events.push_back(HostEvent::SeatRestored { seat });
            }
        }
        let result = match (lane, wire::kind_of(bytes)) {
            (Lane::Datagram, Some(KIND_INPUTS)) => self.on_inputs(seat, bytes, now_ms),
            (Lane::Stream, Some(KIND_REVOTE | KIND_PROBE_ROWS | KIND_SUBSYSTEM_ROWS)) => {
                self.on_seat_stream(seat, bytes)
            }
            _ => Err(LockstepError::Malformed("unexpected payload kind for the lane")),
        };
        if result.is_err() {
            self.rejected += 1;
        }
        result
    }

    fn on_inputs(&mut self, seat: SeatId, bytes: &[u8], now_ms: u64) -> Result<(), LockstepError> {
        let msg = wire::decode_inputs(bytes, self.config.max_input_bytes)?;
        // Decode every distinct input before anything is stored: a datagram with one bad input
        // leaves no trace.
        let mut values: Vec<Option<G::Input>> = Vec::with_capacity(msg.inputs.len());
        for (i, input) in msg.inputs.iter().enumerate() {
            let value = match input {
                None => None,
                Some(b) => match (i.checked_sub(1).and_then(|p| msg.inputs.get(p)), values.last()) {
                    (Some(Some(prev)), Some(Some(v))) if prev == b => Some(v.clone()),
                    _ => Some(G::Input::from_bytes(b)?),
                },
            };
            values.push(value);
        }
        let next_tick = self.next_tick;
        let now_us = now_ms.saturating_mul(1000);
        let Some(origin_us) = self.origin_us else {
            return Err(LockstepError::NotStarted);
        };
        let window_ms = self.config.delay_window_ms;
        let buffer = self.config.input_buffer_ticks as u64;
        // A seat stamps ahead of the frame delay only under `PerSeat`; under `Shared` a reported
        // lead is not a lead the seat may run, and counting it would inflate the seat's measured
        // lateness, the term that raises every seat's delay (ADR-8656).
        let max_lead = match self.config.delay_policy {
            DelayPolicy::PerSeat => self.config.max_extra_lead_ticks,
            DelayPolicy::Shared => 0,
        };
        let voter = Voter::Seat(seat);
        let Some(hs) = self.seats.get_mut(&seat) else {
            return Err(LockstepError::UnknownSeat(seat));
        };
        let was_live = hs.timing.live(now_ms, window_ms);
        hs.timing.heard(now_ms);
        let class = hs.timing.up.on_seq(msg.seq, now_ms);
        // After a catch-up, only datagrams sent once the seat held it speak for its new chain:
        // the first one carries the catching-up flag, and the ones before it are the old chain's.
        if hs.awaiting_catchup && msg.catching_up {
            hs.awaiting_catchup = false;
            hs.catchup_seq = Some(msg.seq);
        }
        let current = !hs.awaiting_catchup && hs.catchup_seq.is_none_or(|first| msg.seq >= first);
        if current {
            hs.acked_next = hs.acked_next.max(msg.ack_next.min(next_tick));
            if let Some(newest) = hs.acked_next.checked_sub(1) {
                let built_us = origin_us
                    .saturating_add(newest.saturating_sub(self.tick_base).saturating_mul(self.tick_us));
                hs.watch.on_ack(hs.acked_next, now_ms, built_us);
            }
        }
        // A repair ends when the seat has the frames it was started at, or when a newer datagram
        // of the current chain no longer reports the hole (the seat's application stopped taking
        // frames). An old-chain datagram does not clear a repair.
        if hs.repair_to.is_some_and(|t| hs.acked_next >= t)
            || (current && !msg.hole && class != SeqClass::Old)
        {
            hs.repair_to = None;
        }
        if current && msg.hole && class != SeqClass::Old && hs.repair_to.is_none() {
            hs.repair_to = Some(next_tick);
            hs.hole_repairs += 1;
        }
        if current && hs.catching_up && !msg.catching_up && msg.ack_next >= hs.catchup_through {
            hs.catching_up = false;
            if !hs.joining {
                self.events.push_back(HostEvent::SeatCaughtUp {
                    seat,
                    tick: next_tick,
                });
            }
        }
        let leave = current && msg.leave && !hs.leaving && hs.left_at.is_none();
        hs.timing.down_p99_run = msg.down_p99_run.min(MAX_RUN);
        hs.window_run = msg.down_window_run.min(MAX_RUN);
        hs.lead = msg.lead.min(max_lead);
        for (tick, hash) in &msg.votes {
            // A seat votes on a tick it stepped, so the host built it first: anything else is
            // not a vote (and must not reach the acknowledgement, which is sent as `tick + 1`).
            if *tick >= next_tick {
                continue;
            }
            self.quorum.vote(voter, *tick, 0, *hash);
            self.quorum.recovered(voter, *tick, *hash);
            hs.vote_ack = Some(hs.vote_ack.map_or(*tick, |t| t.max(*tick)));
        }
        let newest = msg.inputs.len().checked_sub(1);
        let rollback_clock = self.config.mode.window_ticks() > 0;
        for (i, (input, value)) in msg.inputs.iter().zip(values).enumerate() {
            let (Some(bytes), Some(value)) = (input, value) else {
                continue;
            };
            let tick = msg.first_tick.saturating_add(i as u64);
            if tick >= next_tick && tick - next_tick >= buffer {
                hs.too_far += 1;
                continue;
            }
            // A rollback seat's margin is against the frame clock, not the frame the host holds
            // for it: its clock sync steers by it (a held frame would hide that it is late).
            let margin_tick = if rollback_clock {
                next_tick.max(self.tick_base + now_us.saturating_sub(origin_us) / self.tick_us.max(1) + 1)
            } else {
                next_tick
            };
            let first_copy = hs.arrivals.record(tick, margin_tick, now_ms);
            if tick >= next_tick {
                self.driver.push_input(
                    peer(seat),
                    tick,
                    Held {
                        bytes: bytes.clone(),
                        value,
                    },
                );
            } else {
                if first_copy && was_live {
                    self.disturbed = true;
                    self.control.note_disturbance();
                }
                // A tick that was built without this input: it goes into the next frame built,
                // once (spec 120 § 6.3).
                if hs.late.len() < MAX_LATE_PER_SEAT && hs.missed.remove(&tick) {
                    hs.late.push((
                        tick,
                        Held {
                            bytes: bytes.clone(),
                            value,
                        },
                    ));
                }
            }
            // A clean first arrival is a lateness sample: the newest input of an in-order datagram.
            if current && first_copy && Some(i) == newest && class != SeqClass::Old {
                let lead = u64::from(self.stamper.lead_of_stamp(tick).unwrap_or(0)) + u64::from(hs.lead);
                let due = origin_us
                    .saturating_add(tick.saturating_sub(self.tick_base).saturating_mul(self.tick_us));
                let lateness = i128::from(now_us) - i128::from(due) + i128::from(lead * self.tick_us);
                hs.timing.record(
                    now_ms,
                    lateness.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                );
                // The same arrival against the seat's own acknowledgement of the frame that
                // opened the capture, and against the frame it is stamped for.
                let margin = i128::from(due) - i128::from(now_us);
                hs.watch.on_input(
                    tick.saturating_sub(lead),
                    margin.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                    now_ms,
                );
            }
        }
        if leave {
            self.leave_seat(seat, "left", now_ms);
        }
        Ok(())
    }

    /// Removes `seat` at the next frame built and says why (`left` or `timeout`).
    fn leave_seat(&mut self, seat: SeatId, reason: &'static str, now_ms: u64) {
        let Some(hs) = self.seats.get_mut(&seat) else {
            return;
        };
        hs.leaving = true;
        let tick = self.next_tick;
        if hs.joining {
            // Not a member yet: nothing to agree on.
            self.seats.remove(&seat);
        } else {
            self.remove_seat(seat);
        }
        match reason {
            "timeout" => {
                self.seats_timed_out += 1;
                if self.retired.len() >= MAX_RETIRED
                    && let Some(oldest) = self.retired.iter().min_by_key(|(_, at)| **at).map(|(s, _)| *s)
                {
                    self.retired.remove(&oldest);
                }
                self.retired.insert(seat, now_ms);
            }
            _ => self.seats_left += 1,
        }
        self.events
            .push_back(HostEvent::SeatRemoved { seat, tick, reason });
    }

    /// The transport's smoothed round trip on `seat`'s connection, milliseconds (for example
    /// `NetServer::peer_rows`). Optional: with it the late-stamp watch also sees a seat that
    /// holds its frame acknowledgements back, not only its inputs.
    pub fn note_transport_rtt(&mut self, seat: SeatId, rtt_ms: u64) {
        if let Some(hs) = self.seats.get_mut(&seat) {
            hs.watch.note_transport_rtt(rtt_ms);
            hs.rtt_ms = Some(rtt_ms);
        }
    }

    fn on_seat_stream(&mut self, seat: SeatId, bytes: &[u8]) -> Result<(), LockstepError> {
        let voter = Voter::Seat(seat);
        match wire::decode_seat_stream(bytes)? {
            SeatStream::Revote { tick, round, hash } => {
                self.quorum.vote(voter, tick, round, hash);
                self.quorum.recovered(voter, tick, hash);
            }
            SeatStream::ProbeRows { from, hashes } => self.probe_rows(voter, from, hashes),
            SeatStream::SubsystemRows(rows) => self.subsystem_rows_from(voter, rows),
        }
        Ok(())
    }

    /// The player host's own input for the open capture tick (a listen host only). Under
    /// [`HostInput::Delayed`] it is stamped `capture tick + delay` like a seat's; under
    /// [`HostInput::Immediate`] it goes into the next frame. One input per capture tick: a second
    /// call before the next frame is refused with [`LockstepError::CaptureClosed`]. The host's
    /// seat has to be added with [`Self::add_seat`] first ([`LockstepError::UnknownSeat`]).
    pub fn local_input(&mut self, input: &G::Input, now_ms: u64) -> Result<Stamp, LockstepError> {
        let Some(seat) = self.role.local_seat() else {
            return Err(LockstepError::NoLocalSeat);
        };
        let Some(origin_us) = self.origin_us else {
            return Err(LockstepError::NotStarted);
        };
        // The host's own seat has to be added like any other: without it no frame lists the
        // input, and answering with a tick would promise one.
        if !self.seats.contains_key(&seat) {
            return Err(LockstepError::UnknownSeat(seat));
        }
        if self.ended_at.is_some() {
            return Err(LockstepError::MatchEnded);
        }
        if self.local_used {
            return Err(LockstepError::CaptureClosed);
        }
        let stamp = match self.config.host_input {
            // A gap tick's frame carries no input for anyone, the host included.
            HostInput::Immediate if self.stamper.is_gap(self.next_tick) => Stamp::Skipped,
            HostInput::Immediate => Stamp::Tick(self.next_tick),
            HostInput::Delayed => self.open_capture.unwrap_or(Stamp::Skipped),
        };
        self.local_used = true;
        let Stamp::Tick(tick) = stamp else {
            return Ok(stamp);
        };
        let bytes = input.to_bytes()?;
        if bytes.len() > self.config.max_input_bytes {
            self.local_used = false;
            return Err(LockstepError::InputTooLarge {
                len: bytes.len(),
                max: self.config.max_input_bytes,
            });
        }
        // The host steps what the wire carries, like every seat: decode the bytes it will send.
        let value = G::Input::from_bytes(&bytes)?;
        let next_tick = self.next_tick;
        let lead = match self.config.host_input {
            HostInput::Immediate => 0,
            HostInput::Delayed => u64::from(self.stamper.lead_of_stamp(tick).unwrap_or(0)),
        };
        if let Some(hs) = self.seats.get_mut(&seat) {
            hs.timing.heard(now_ms);
            if hs.arrivals.record(tick, next_tick, now_ms) {
                let due = origin_us
                    .saturating_add(tick.saturating_sub(self.tick_base).saturating_mul(self.tick_us));
                let lateness = i128::from(now_ms.saturating_mul(1000)) - i128::from(due)
                    + i128::from(lead * self.tick_us);
                hs.timing.record(
                    now_ms,
                    lateness.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                );
            }
        }
        self.driver.push_input(peer(seat), tick, Held { bytes, value });
        Ok(stamp)
    }

    /// Under rollback, a seat whose median input over the last feedback window arrived more than
    /// the rollback window after its frame was due is no longer held for: holding cannot bring
    /// most of its inputs in, and every hold past the window makes the other seats wait at their
    /// window's edge. Its inputs that come after their frame are merged late, as for any late
    /// input; it pays for its own link. It is held for again once its median margin is two ticks
    /// inside the window (KI-NT-038, ADR-8855), measured over as many arrivals as the release
    /// needs: a feedback window thinned by a loss burst keeps the seat released (ADR-10147), or
    /// the other seats would wait out the two-window grace while the host gathers fresh arrivals.
    fn judge_hold_release(&mut self, now_ms: u64) {
        let window = i32::from(self.config.mode.window_ticks());
        for (seat, hs) in &mut self.seats {
            let (p50, samples) = hs.arrivals.margin_p50_at(now_ms);
            let enough = samples >= HOLD_RELEASE_SAMPLES;
            if !hs.hold_released && enough && p50 < -window {
                hs.hold_released = true;
                tracing::info!(%seat, margin_p50_ticks = p50, "lockstep host: no longer holding frames for a seat past the window");
            } else if hs.hold_released && enough && p50 >= 2 - window {
                hs.hold_released = false;
                tracing::info!(%seat, margin_p50_ticks = p50, "lockstep host: holding frames for the seat again");
            }
        }
    }

    /// Builds the next frame when it is due at `now_ms` and returns it, for the host's own
    /// simulation. Call until it returns `None`. The host never builds ahead of its clock.
    pub fn poll_frame(&mut self, now_ms: u64) -> Option<LockstepFrame<G::Input>> {
        let _span = tracing::trace_span!("regia_net::lockstep_build").entered();
        let tick = self.next_tick;
        let now_us = now_ms.saturating_mul(1000);
        let due = self.due_us(tick)?;
        if now_us < due || self.ended_at.is_some() {
            return None;
        }
        let gap = self.stamper.is_gap(tick);
        let window_ms = self.config.delay_window_ms;
        // Rollback lockstep holds a frame for a live seat's input for up to twice the rollback
        // window: for the first window the other seats run ahead on predictions, so the wait
        // costs them a rollback, then they wait at the window's edge (the GGPO model: an input
        // is waited for, not dropped). Past that the seat's input is held and merged late
        // (spec 120 § 7.7, ADR-8361).
        let window_us = u64::from(self.config.mode.window_ticks())
            .saturating_mul(2)
            .saturating_mul(self.tick_us);
        let grace_us = self.config.late_grace_ms.saturating_mul(1000).max(window_us);
        let rollback = window_us > 0;
        if rollback && self.config.release_slow_seat_hold {
            self.judge_hold_release(now_ms);
        }
        if grace_us > 0 && !gap {
            let live: Vec<PeerId> = self
                .members
                .iter()
                .filter(|s| {
                    self.seats.get(s).is_some_and(|hs| {
                        // A rollback seat is waited for while it was heard within the window, or
                        // admitted within it and not heard yet: its first inputs are on the way
                        // when the match starts.
                        let heard = rollback && hs.silent_ms(now_ms) <= window_ms;
                        (hs.timing.live(now_ms, window_ms) || heard) && !hs.interrupted && !hs.hold_released
                    })
                })
                .map(|s| peer(*s))
                .collect();
            if !live.is_empty() && !self.driver.ready_for(tick, live) && now_us < due.saturating_add(grace_us)
            {
                return None;
            }
        }

        self.judge_silence(tick, now_ms);
        // Membership changes apply at this tick (a frame carries a bounded number; the rest wait
        // for the next one, with the match end marker counted).
        let limit = MAX_MEMBERSHIP_PER_FRAME - 1;
        let rest = if self.pending.len() > limit {
            self.pending.split_off(limit)
        } else {
            Vec::new()
        };
        let membership = std::mem::replace(&mut self.pending, rest);
        if !membership.is_empty() {
            self.matrix_dirty = true;
            self.roster_dirty = true;
        }
        for change in &membership {
            match change.kind {
                MembershipKind::Join => {
                    self.members.insert(change.seat);
                    self.driver.add_peer(peer(change.seat));
                }
                MembershipKind::Leave => {
                    self.members.remove(&change.seat);
                    self.driver.remove_peer(peer(change.seat));
                    self.quorum.forget(Voter::Seat(change.seat));
                    if let Some(hs) = self.seats.get_mut(&change.seat) {
                        hs.left_at = Some(tick);
                    }
                }
            }
        }

        let mut taken = self.driver.advance(|_, _| {});
        // A gap tick's frame carries no input. Under `PerSeat` a seat that stamps with its own
        // lead can have an input on a tick the shared schedule leaves empty (it was stamped
        // before the raise): the frame then lists inputs like any other, and only the seats
        // without one are absent. Under `Shared` no honest seat can stamp a gap tick.
        let carries = !gap || (self.config.delay_policy == DelayPolicy::PerSeat && !taken.is_empty());
        let rules = self.rules;
        let max_input = self.config.max_input_bytes;
        let mut entries = Vec::with_capacity(self.members.len());
        let mut values = Vec::with_capacity(self.members.len());
        for seat in &self.members {
            let mut held = if carries { taken.remove(&peer(*seat)) } else { None };
            if let Some(hs) = self.seats.get_mut(seat) {
                // The seat's own input for this tick is not here: when it arrives, it merges into
                // a later frame, even if this one carries other late inputs of the seat.
                // Under `PerSeat` a seat with its own lead may have stamped even a gap tick.
                if held.is_none() && (carries || self.config.delay_policy == DelayPolicy::PerSeat) {
                    if hs.missed.len() >= MAX_MISSED_PER_SEAT {
                        hs.missed.pop_first();
                    }
                    hs.missed.insert(tick);
                }
                if !gap && !hs.late.is_empty() {
                    // Inputs that missed their ticks go into this one (spec 120 § 6.3); a gap tick
                    // of a delay raise stays empty for every seat (§ 7.3), they wait for the next.
                    let late = std::mem::take(&mut hs.late);
                    hs.late_merged += late.len() as u64;
                    held = merge_late(rules, late, held, max_input);
                }
                if held.is_none() && !gap && tick >= hs.count_missing_from {
                    hs.missing += 1;
                    if hs.timing.live(now_ms, window_ms) {
                        self.disturbed = true;
                        self.control.note_disturbance();
                    }
                }
                hs.arrivals.note_missed(hs.missing);
            }
            match held {
                Some(h) => {
                    entries.push((*seat, Some(h.bytes)));
                    values.push((*seat, Some(h.value)));
                }
                None => {
                    if carries {
                        entries.push((*seat, None));
                    }
                    values.push((*seat, None));
                }
            }
        }

        // The delay decision rides in this frame and starts at the next capture tick.
        let mut delay_change = None;
        let check_every = u64::from(self.config.tick_rate_hz / 4).max(1);
        if self.disturbed || tick.saturating_sub(self.last_check_tick) >= check_every {
            self.disturbed = false;
            self.last_check_tick = tick;
            let tick_us = self.tick_us;
            let policy = self.config.delay_policy;
            let mut needs: Vec<u16> = Vec::with_capacity(self.members.len());
            for seat in &self.members {
                if let Some(hs) = self.seats.get_mut(seat)
                    && let Some(need) = hs.timing.need(now_ms, window_ms, tick_us, policy)
                {
                    needs.push(need);
                }
            }
            let current = self.stamper.current().0;
            // A rollback seat may have sampled up to its window past the frames it has: the
            // change starts after every capture any seat can have made.
            let from = tick + 1 + u64::from(self.config.mode.window_ticks());
            if let Some(delay) = self.control.decide(now_ms, current, &mut needs)
                && self.stamper.set_delay(from, delay)
            {
                delay_change = Some((delay, from));
                self.events.push_back(HostEvent::DelayChanged {
                    tick,
                    delay,
                    delay_from: from,
                    raised: delay > current,
                });
            }
        }

        let match_end = std::mem::take(&mut self.end_pending);
        // Once a second: the verdict on each seat's input timing (late stamping).
        if tick.is_multiple_of(u64::from(self.config.tick_rate_hz)) {
            let local = self.role.local_seat();
            for seat in &self.members {
                if Some(*seat) != local
                    && let Some(hs) = self.seats.get_mut(seat)
                    && let Some(change) = hs.watch.judge(now_ms, self.tick_us)
                {
                    self.events.push_back(HostEvent::LateStamp {
                        seat: *seat,
                        suspect: change.suspect,
                        hold_us: change.hold_us,
                        margin_us: change.margin_us,
                    });
                }
            }
        }

        let raw = RawFrame {
            tick,
            delay_change,
            membership: membership.clone(),
            gap: !carries,
            entries,
            match_end,
        };
        let body = wire::encode_frame_body(&raw, &mut self.state);
        // A seat without an input steps with what the missing-input policy holds for it, derived
        // from the frame chain the same way on every participant.
        let policy = self.config.missing_input;
        let mut inputs = Vec::with_capacity(values.len());
        for (seat, value) in values {
            let input = match value {
                Some(v) => SeatInput::Present(v),
                None if carries => {
                    let held = held_input::<G::Input>(policy, rules, self.state.seats.get(&seat));
                    if held.is_some()
                        && let Some(hs) = self.seats.get_mut(&seat)
                    {
                        hs.held += 1;
                    }
                    held.map_or(SeatInput::Absent, SeatInput::Held)
                }
                None => SeatInput::Absent,
            };
            inputs.push((seat, input));
        }
        let frame = LockstepFrame {
            tick,
            inputs,
            delay: self.state.delay,
            delay_from: self.state.delay_from,
            membership,
            match_end,
        };
        if match_end {
            self.ended_at = Some(tick);
            self.events.push_back(HostEvent::MatchEnded { tick });
        }
        self.log.push_back(Logged {
            tick,
            body,
            frame: frame.clone(),
        });
        self.next_tick = tick + 1;
        self.open_capture = Some(self.stamper.stamp_next(0).1);
        self.local_used = false;
        // Seats added again while their leave was pending: the leave is in this frame, the join
        // goes into the next.
        for seat in std::mem::take(&mut self.rejoin) {
            self.admit(seat);
        }
        self.stamper
            .prune(tick.saturating_sub(self.config.input_buffer_ticks as u64));

        if self.config.hash_every > 0 && tick.is_multiple_of(u64::from(self.config.hash_every)) {
            let voters: Vec<Voter> = self
                .members
                .iter()
                .filter(|s| self.seats.get(s).is_some_and(|hs| !hs.lagging))
                .map(|s| Voter::Seat(*s))
                .collect();
            self.quorum.open(tick, voters.into_iter(), tick);
        }
        self.run_quorum();
        self.trim_log();

        // The host's simulation steps next: its thread's float environment must be the default.
        if !regia_detsim::fpu::ensure_default_fp_env().was_default() {
            self.fp_env_corrections += 1;
        }
        Some(frame)
    }

    /// Silence by the session's timeout policy (spec 120 § 7.4, § 10.1): an interruption notice,
    /// removal at the timeout (never before the start grace for a seat not heard yet), and the
    /// join of a seat that has stepped a catch-up to the live edge. Changes go into the frame at
    /// `tick`.
    fn judge_silence(&mut self, tick: u64, now_ms: u64) {
        let policy = self.config.timeout;
        let local = self.role.local_seat();
        let retire_before = now_ms.saturating_sub(self.config.resume_window_ms);
        self.retired.retain(|_, at| *at >= retire_before);
        let lead = u64::from(self.stamper.current().0) + u64::from(self.config.max_extra_lead_ticks);
        let mut timed_out = Vec::new();
        let mut joins = Vec::new();
        for (seat, hs) in &mut self.seats {
            if Some(*seat) == local || hs.left_at.is_some() || hs.leaving {
                continue;
            }
            let admitted = *hs.admitted_ms.get_or_insert(now_ms);
            let last = hs.last_heard_ms.unwrap_or(admitted);
            if policy.expired(now_ms, admitted, last, hs.heard_any) {
                timed_out.push(*seat);
                continue;
            }
            let counting = hs.heard_any || now_ms.saturating_sub(admitted) >= policy.start_grace_ms;
            if counting && !hs.interrupted && now_ms.saturating_sub(last) >= policy.interrupt_ms {
                hs.interrupted = true;
                self.matrix_dirty = true;
                self.events.push_back(HostEvent::SeatInterrupted { seat: *seat });
            }
            if hs.joining && hs.heard_any && !hs.catching_up {
                hs.joining = false;
                // Its first capture is the tick after the join: earlier ticks are not missing.
                hs.count_missing_from = tick + 1 + lead;
                joins.push(*seat);
            }
        }
        for seat in timed_out {
            self.leave_seat(seat, "timeout", now_ms);
        }
        for seat in joins {
            self.pending.push(MembershipChange {
                seat,
                kind: MembershipKind::Join,
            });
            self.quorum.admit(Voter::Seat(seat));
            self.events.push_back(HostEvent::SeatJoined { seat, tick });
            self.events.push_back(HostEvent::SeatCaughtUp { seat, tick });
        }
    }

    fn trim_log(&mut self) {
        let keep = self.config.frame_log_ticks as u64;
        let max_input = self.config.max_input_bytes;
        while self.log.front().is_some_and(|l| l.tick + keep < self.next_tick) {
            if let Some(old) = self.log.pop_front() {
                // The base moves past the frame: a catch-up can start at the log's new front.
                let mut base = self.log_base.clone();
                if wire::decode_frame_body(&old.body, old.tick, &mut base, max_input).is_ok() {
                    self.log_base = base;
                } else {
                    self.log_base_broken = true;
                }
            }
        }
        let Some(front) = self.log.front().map(|l| l.tick) else {
            return;
        };
        let next_tick = self.next_tick;
        let local = self.role.local_seat();
        let mut gone = Vec::new();
        for (seat, hs) in &mut self.seats {
            if Some(*seat) == local {
                // The host's own seat steps from `poll_frame`.
                hs.acked_next = next_tick;
                continue;
            }
            if let Some(left) = hs.left_at
                && (hs.acked_next > left || next_tick > left + LEFT_LINGER_TICKS)
            {
                gone.push(*seat);
                continue;
            }
            if !hs.lagging && hs.after_stream(hs.acked_next) < front {
                hs.lagging = true;
                self.events.push_back(HostEvent::SeatLagging { seat: *seat });
            }
        }
        for seat in gone {
            self.seats.remove(&seat);
        }
    }

    /// The hash of the host's own simulation after it stepped `tick`. Report it every tick: the
    /// checkpoint ticks are votes, the rest feed the probe that finds the first diverged tick.
    /// A tick reported again replaces its hash in the probe's ring.
    pub fn report_hash(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
        if let Some(voter) = self.role.voter()
            && self.config.hash_every > 0
            && tick.is_multiple_of(u64::from(self.config.hash_every))
        {
            self.quorum.vote(voter, tick, 0, hash);
            self.quorum.recovered(voter, tick, hash);
            self.run_quorum();
        }
    }

    /// Hashes the host's own simulation after it stepped `tick` live ([`DesyncHash`]): the
    /// subsystem rows go into the host's ring for the probe, and the world hash is reported as by
    /// [`Self::report_hash`]. Returns the world hash. Use it instead of `report_hash`.
    pub fn record_state<H: DesyncHash + ?Sized>(&mut self, tick: u64, state: &H) -> u64 {
        let world = self.rows.record(tick, state);
        self.report_hash(tick, world);
        world
    }

    /// Hashes the host's simulation after it stepped `tick` again in a replay: the ring's rows from
    /// `tick` on are replaced and the world hash replaces `tick`'s in the probe's hash ring, no
    /// vote is cast. Returns the world hash (for [`Self::revote`]).
    pub fn record_replayed<H: DesyncHash + ?Sized>(&mut self, tick: u64, state: &H) -> u64 {
        let world = self.rows.record(tick, state);
        put_ring_hash(&mut self.hash_ring, tick, world);
        world
    }

    /// The hash of the host's simulation after it stepped `tick` again in a replay (a
    /// [`HostEvent::SelfCheck`], or the frames after a [`HostEvent::ResyncHost`] state), for a
    /// game that reports plain hashes ([`Self::report_hash`]) rather than [`Self::record_replayed`]:
    /// it replaces `tick`'s hash in the probe's ring and casts no vote. Report every replayed tick,
    /// so a later probe compares the history the simulation has now (ADR-10147).
    pub fn report_replayed_hash(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
    }

    /// Takes the observer's vote out of the quorum (`false`) or puts it back (`true`); only for
    /// [`HostRole::Observer`], else nothing happens and it returns `false` (ADR-8558).
    ///
    /// An observer that steps on its own thread and falls behind leaves the quorum, so no tally
    /// waits for it until its deadline; the seats never wait for it either way. Out of the quorum
    /// it is expected at no checkpoint and its votes are not counted. Back in, its record starts
    /// clean and it is expected from the next checkpoint built on.
    pub fn set_observer_voting(&mut self, voting: bool) -> bool {
        if self.role != HostRole::Observer {
            return false;
        }
        if voting {
            self.quorum.admit(Voter::Observer);
        } else {
            self.quorum.forget(Voter::Observer);
            // Checkpoints that were waiting only for the observer close now.
            self.run_quorum();
        }
        true
    }

    /// The subsystem rows of the host's own simulation.
    pub fn subsystem_rows(&self) -> &SubsystemRing {
        &self.rows
    }

    /// The subsystem rows the newest probe collected (each diverged voter's and the reference's,
    /// around each first diverged tick), ascending by voter: what a desync bundle carries
    /// ([`crate::desync::DesyncBundle::from_host`]). Empty before the first probe.
    pub fn desync_probe_rows(&self) -> Vec<(Voter, ProbeRows)> {
        self.subsystem_probe
            .as_ref()
            .map(SubsystemProbe::peer_rows)
            .unwrap_or_default()
    }

    /// The probe reports so far, and the newest.
    pub fn desync_reports(&self) -> (u64, Option<&DesyncReport>) {
        (self.desync_reports, self.last_desync.as_ref())
    }

    /// The host's vote on `tick` again, after a [`HostEvent::SelfCheck`] replay or after it took
    /// the state of a [`HostEvent::ResyncHost`] and replayed to `tick`. The hash also replaces
    /// `tick`'s in the probe's ring; the replayed ticks around it go in with
    /// [`Self::record_replayed`] or [`Self::report_replayed_hash`].
    pub fn revote(&mut self, tick: u64, hash: u64) {
        put_ring_hash(&mut self.hash_ring, tick, hash);
        if let Some(voter) = self.role.voter() {
            self.quorum.vote(voter, tick, 1, hash);
            self.quorum.recovered(voter, tick, hash);
            self.run_quorum();
        }
    }

    /// The frames from `from_tick` on that the host still holds, oldest first: what a replay from
    /// a checkpoint, a state transfer or a late join steps (the host's side of the seam).
    pub fn frames_since(&self, from_tick: u64) -> impl Iterator<Item = &LockstepFrame<G::Input>> {
        self.log
            .iter()
            .filter(move |l| l.tick >= from_tick)
            .map(|l| &l.frame)
    }

    fn run_quorum(&mut self) {
        let host = self.role.voter();
        for action in self.quorum.advance(self.next_tick) {
            match action {
                QuorumAction::Recheck { tick, from, voters } => {
                    for voter in voters {
                        match voter {
                            v if Some(v) == host => {
                                self.events.push_back(HostEvent::SelfCheck { from, tick })
                            }
                            Voter::Seat(seat) => self.order(seat, Order::Recheck { tick, from }),
                            Voter::Observer => {}
                        }
                    }
                }
                QuorumAction::Verdict(verdict) => {
                    self.start_probe(&verdict);
                    self.events.push_back(HostEvent::Desync(verdict));
                }
                QuorumAction::Resync {
                    tick,
                    target,
                    source,
                    counted,
                    resyncs,
                } => {
                    let wire_of = |v: Voter| match v {
                        Voter::Seat(seat) => Some(seat),
                        Voter::Observer => None,
                    };
                    if Some(target) == host {
                        self.events.push_back(HostEvent::ResyncHost {
                            source,
                            tick,
                            resyncs,
                            counted,
                        });
                    } else if let Voter::Seat(seat) = target {
                        self.events.push_back(HostEvent::ResyncSeat {
                            seat,
                            source,
                            tick,
                            resyncs,
                            counted,
                        });
                        self.order(
                            seat,
                            Order::Resync {
                                tick,
                                source: wire_of(source),
                            },
                        );
                    }
                    if Some(source) != host
                        && let Voter::Seat(source_seat) = source
                    {
                        self.order(
                            source_seat,
                            Order::ProvideState {
                                tick,
                                target: wire_of(target),
                            },
                        );
                    }
                }
                QuorumAction::Remove { tick, seat } => {
                    self.order(seat, Order::Removed { tick });
                    self.remove_seat(seat);
                    self.events.push_back(HostEvent::SeatRemoved {
                        seat,
                        tick,
                        reason: "desync",
                    });
                }
                QuorumAction::HostFault { tick, resyncs } => {
                    self.events.push_back(HostEvent::HostFault { tick, resyncs });
                }
                QuorumAction::Unresolved { tick } => {
                    let seats: Vec<SeatId> = self.seats.keys().copied().collect();
                    for seat in seats {
                        self.order(seat, Order::Unresolved { tick });
                    }
                    self.events.push_back(HostEvent::Unresolved { tick });
                }
            }
        }
    }

    fn order(&mut self, seat: SeatId, order: Order) {
        if Some(seat) == self.role.local_seat() {
            return;
        }
        if let Some(hs) = self.seats.get_mut(&seat) {
            hs.orders.push(order);
        }
    }

    /// After a verdict with diverged voters: ask them (at most `SubsystemProbe::MAX_TARGETS`) and
    /// the source for their per-tick hashes since the last agreed checkpoint, to name the first
    /// tick each differs at; then their subsystem rows there (spec 120 § 7.6).
    fn start_probe(&mut self, verdict: &DesyncVerdict) {
        if verdict.kind != VerdictKind::Diverged {
            return;
        }
        let Some(reference) = verdict.source else {
            return;
        };
        let mut diverged = verdict.diverged.clone();
        // Probed in voter order on the live path, as the offline replay walks them (spec 120 § 7.6).
        diverged.sort_unstable();
        diverged.dedup();
        diverged.retain(|v| *v != reference);
        diverged.truncate(SubsystemProbe::MAX_TARGETS);
        if diverged.is_empty() {
            return;
        }
        let from = verdict
            .last_agreed
            .map_or(0, |t| t + 1)
            .max((verdict.tick + 1).saturating_sub(HASH_RING_TICKS as u64));
        let mut job = ProbeJob {
            tick: verdict.tick,
            from,
            reference,
            rows: BTreeMap::new(),
            diverged: diverged.clone(),
            located: BTreeSet::new(),
        };
        let host = self.role.voter();
        self.probe_hold = (!self.rows.is_empty()).then(|| self.rows.window(from, verdict.tick));
        self.subsystem_probe = Some(SubsystemProbe::new(verdict.tick, verdict.last_agreed, reference));
        for voter in std::iter::once(reference).chain(diverged) {
            if Some(voter) == host {
                let rows: Vec<u64> = (from..=verdict.tick)
                    .map_while(|t| {
                        self.hash_ring
                            .iter()
                            .find(|(tick, _)| *tick == t)
                            .map(|(_, h)| *h)
                    })
                    .collect();
                job.rows.insert(voter, rows);
            } else if let Voter::Seat(seat) = voter {
                self.order(
                    seat,
                    Order::Probe {
                        from,
                        to: verdict.tick,
                    },
                );
            }
        }
        self.probe = Some(job);
        self.finish_probe();
    }

    fn probe_rows(&mut self, voter: Voter, from: u64, hashes: Vec<u64>) {
        if let Some(job) = &mut self.probe
            && from == job.from
            && (voter == job.reference || job.diverged.contains(&voter))
        {
            job.rows.entry(voter).or_insert(hashes);
        }
        self.finish_probe();
    }

    fn finish_probe(&mut self) {
        let Some(job) = &mut self.probe else {
            return;
        };
        let Some(reference) = job.rows.get(&job.reference) else {
            return;
        };
        let mut located = Vec::new();
        for voter in &job.diverged {
            if job.located.contains(voter) {
                continue;
            }
            let Some(rows) = job.rows.get(voter) else {
                continue;
            };
            let split = rows
                .iter()
                .zip(reference)
                .position(|(x, y)| x != y)
                .map(|i| job.from + i as u64);
            located.push((*voter, split));
        }
        let tick = job.tick;
        for (voter, _) in &located {
            job.located.insert(*voter);
        }
        if job.located.len() == job.diverged.len() {
            self.probe = None;
        }
        for (voter, split) in located {
            let Some(first_diverged_tick) = split else {
                continue;
            };
            self.events.push_back(HostEvent::DesyncLocated {
                tick,
                voter,
                first_diverged_tick,
            });
            self.probe_subsystems(voter, first_diverged_tick);
        }
    }

    /// The probe's second stage for `voter`: the subsystem rows at its first diverged tick and the
    /// tick before it, from it and from the reference.
    fn probe_subsystems(&mut self, voter: Voter, split: u64) {
        let Some(probe) = &mut self.subsystem_probe else {
            return;
        };
        let Some((from, to)) = probe.add_target(voter, split) else {
            return;
        };
        let reference = probe.reference;
        let host = self.role.voter();
        for asked in [reference, voter] {
            if Some(asked) == host {
                let rows = self.own_rows(from, to);
                if let Some(probe) = &mut self.subsystem_probe {
                    probe.offer(asked, rows);
                }
            } else if let Voter::Seat(seat) = asked {
                self.order(seat, Order::ProbeSubsystems { from, to });
            }
        }
        self.finish_subsystem_probe();
    }

    /// The host's rows of `from..=to`: from the copy kept when the probe started when it holds
    /// them, else from the ring.
    fn own_rows(&self, from: u64, to: u64) -> ProbeRows {
        if let Some(held) = &self.probe_hold
            && (from..=to).all(|t| held.rows.iter().any(|row| row.tick == t))
        {
            return ProbeRows {
                names: held.names.clone(),
                rows: held
                    .rows
                    .iter()
                    .filter(|row| (from..=to).contains(&row.tick))
                    .cloned()
                    .collect(),
            };
        }
        self.rows.window(from, to)
    }

    fn subsystem_rows_from(&mut self, voter: Voter, rows: ProbeRows) {
        // Rows nobody asked for (a stale or a hostile reply) are dropped.
        if let Some(probe) = &mut self.subsystem_probe {
            probe.offer(voter, rows);
        }
        self.finish_subsystem_probe();
    }

    fn finish_subsystem_probe(&mut self) {
        let Some(probe) = &mut self.subsystem_probe else {
            return;
        };
        for report in probe.reports() {
            self.desync_reports += 1;
            self.last_desync = Some(report.clone());
            self.events.push_back(HostEvent::DesyncProbed(report));
        }
    }

    /// The payloads to send now: per seat, one frames datagram with the newest frame and every
    /// frame the seat has not acknowledged, the stream payloads for frames that did not fit, and
    /// any quorum orders. Call once per host tick, after [`Self::poll_frame`].
    pub fn outgoing(&mut self, now_ms: u64) -> Vec<(SeatId, Outgoing)> {
        let _span = tracing::trace_span!("regia_net::lockstep_send").entered();
        self.queue_migration_info();
        let mut out = Vec::new();
        let next_tick = self.next_tick;
        let local = self.role.local_seat();
        let budget = self.config.datagram_budget_bytes;
        let buffer = self.config.input_buffer_ticks;
        let redundancy = self.config.redundancy;
        let log_ticks = self.config.frame_log_ticks as u64;
        let Some(front) = self.log.front().map(|l| l.tick) else {
            return out;
        };
        for (seat, hs) in &mut self.seats {
            if Some(*seat) == local {
                // The host's own seat steps from `poll_frame`.
                hs.acked_next = next_tick;
                continue;
            }
            for bytes in hs.stream_out.drain(..) {
                self.stream_bytes_sent += bytes.len() as u64;
                out.push((
                    *seat,
                    Outgoing {
                        lane: Lane::Stream,
                        bytes,
                    },
                ));
            }
            for order in hs.orders.drain(..) {
                let bytes = wire::encode_order(&order);
                self.stream_bytes_sent += bytes.len() as u64;
                out.push((
                    *seat,
                    Outgoing {
                        lane: Lane::Stream,
                        bytes,
                    },
                ));
            }
            // A seat whose acknowledgement trails the stream by a whole frame log is not stepping:
            // stop feeding it.
            if !hs.lagging && hs.stream_next.saturating_sub(hs.acked_next) > log_ticks {
                hs.lagging = true;
                self.events.push_back(HostEvent::SeatLagging { seat: *seat });
            }
            if hs.lagging {
                continue;
            }
            let end = hs.left_at.map_or(next_tick, |left| (left + 1).min(next_tick));
            // The first frame the seat may still need that the stream does not already carry.
            let unacked = hs.after_stream(hs.acked_next);
            let mut start = unacked;
            if redundancy != FrameRedundancy::AllUnacked && hs.repair_to.is_none() {
                // Repeat only what a loss run of the link's length can have taken, and never
                // skip a frame that was not sent yet (several frames built between two sends).
                let depth = window_depth(
                    redundancy,
                    hs.window_run,
                    hs.out_seq.wrapping_add(u64::from(seat.0)),
                );
                start = start.max(end.saturating_sub(depth).min(hs.sent_next));
            }
            let lag = next_tick.saturating_sub(hs.acked_next);
            hs.lag_max = hs.lag_max.max(lag);
            // A seat whose acknowledgement is older than the frame log cannot be served (it is
            // marked lagging when the log is trimmed).
            if unacked < front {
                continue;
            }
            let feedback_due = hs.arrivals.active()
                && (hs.arrivals.urgent_due(next_tick)
                    || hs
                        .feedback_at_ms
                        .is_none_or(|at| now_ms.saturating_sub(at) >= FEEDBACK_INTERVAL_MS));
            let feedback = feedback_due.then(|| {
                hs.feedback_at_ms = Some(now_ms);
                let upstream_loss = hs
                    .loss
                    .update(hs.timing.up.newest().map(|s| s as u16), hs.timing.up.received());
                hs.arrivals
                    .feedback(next_tick, now_ms, hs.too_far, buffer, upstream_loss)
            });
            if start >= end && feedback.is_none() {
                continue;
            }
            let mut header = FramesHeader {
                seq: hs.out_seq,
                next_build: next_tick,
                vote_ack: hs.vote_ack,
                base_tick: start,
                feedback,
            };
            let log = &self.log;
            let body_of = |tick: u64| body_at(log, front, tick);
            // Frames that do not fit the datagram go on the stream, oldest first, once. The header
            // is sized for the largest base tick it can end up with.
            header.base_tick = end;
            let header_len = wire::frames_header_len(&header);
            let mut size: usize = (start..end).map(|t| wire::body_cost(body_of(t))).sum();
            // Under interleaved redundancy a deeper datagram that does not fit drops its oldest
            // repeats instead: they were sent before, a seat that lacks them reports the hole,
            // and the stream is for frames no datagram can carry. The newest frame and the one
            // before it stay (one lost datagram is still repaired by the next).
            if redundancy == FrameRedundancy::Interleaved && hs.repair_to.is_none() {
                let repeats_end = hs.sent_next.min(end.saturating_sub(2));
                while start < repeats_end && header_len + size > budget {
                    size -= wire::body_cost(body_of(start));
                    start += 1;
                }
            }
            let mut to_stream = start;
            while to_stream < end
                && (header_len + size > budget || (end - to_stream) as usize > MAX_FRAMES_PER_PAYLOAD)
            {
                size -= wire::body_cost(body_of(to_stream));
                to_stream += 1;
            }
            if to_stream > start {
                // The stream delivers once and in order, so a run has to begin at a frame the
                // seat is known to have reached: at the acknowledgement, or where the last run
                // ended. Beginning at the window's start would hand the seat frames it cannot
                // apply when the datagrams before them were lost.
                start = unacked;
                if start != hs.stream_next {
                    hs.stream_base = start;
                }
                hs.stream_next = to_stream;
            }
            for (count, bytes) in stream_batches(log, front, start, to_stream) {
                self.stream_bytes_sent += bytes.len() as u64;
                hs.stream_frames += count as u64;
                out.push((
                    *seat,
                    Outgoing {
                        lane: Lane::Stream,
                        bytes,
                    },
                ));
            }
            start = start.max(to_stream);
            header.base_tick = start;
            let count = end.saturating_sub(start) as usize;
            let bytes = wire::encode_frames_datagram(&header, count, (start..end).map(body_of));
            hs.sent_next = hs.sent_next.max(end);
            hs.out_seq += 1;
            self.datagrams_sent += 1;
            self.datagram_bytes_sent += bytes.len() as u64;
            out.push((
                *seat,
                Outgoing {
                    lane: Lane::Datagram,
                    bytes,
                },
            ));
        }
        out
    }

    /// Stops feeding `seat`: the transport lost frames meant for its stream, so it can only
    /// return through a state transfer (a [`HostEvent::SeatLagging`] follows once).
    pub fn mark_lagging(&mut self, seat: SeatId) {
        if let Some(hs) = self.seats.get_mut(&seat)
            && !hs.lagging
        {
            hs.lagging = true;
            self.events.push_back(HostEvent::SeatLagging { seat });
        }
    }

    /// The next thing the application has to know or do.
    pub fn poll_event(&mut self) -> Option<HostEvent> {
        self.events.pop_front()
    }

    /// Whether the votes could not be decided and the match should end (`desync_unresolved`).
    pub fn unresolved(&self) -> bool {
        self.quorum.is_unresolved()
    }

    /// What `net.lockstep` reports for the host.
    pub fn status(&mut self, now_ms: u64) -> LockstepStatus {
        let (delay, delay_from) = self.stamper.current();
        let window_ms = self.config.delay_window_ms;
        let tick_us = self.tick_us;
        let policy = self.config.delay_policy;
        let next_tick = self.next_tick;
        let local = self.role.local_seat();
        let mut seats = Vec::with_capacity(self.seats.len());
        for (seat, hs) in &mut self.seats {
            let record = self.quorum.record(Voter::Seat(*seat));
            let lag = if Some(*seat) == local {
                0
            } else {
                next_tick.saturating_sub(hs.acked_next)
            };
            seats.push(SeatLockstepStatus {
                seat: seat.0,
                frame_lag_ticks: lag,
                frame_lag_max_ticks: hs.lag_max.max(lag),
                inputs_missing: hs.missing,
                inputs_late: hs.arrivals.late_total(),
                inputs_too_far_ahead: hs.too_far,
                lateness_p99_us: hs.timing.lateness_p99(now_ms, window_ms),
                loss_run_p99: hs.timing.loss_run_p99(now_ms),
                delay_need_ticks: hs.timing.need(now_ms, window_ms, tick_us, policy),
                extra_lead_ticks: hs.lead,
                resyncs: record.resyncs,
                self_heals: record.self_heals,
                resyncing: record.resyncing.is_some(),
                stream_frames: hs.stream_frames,
                hole_repairs: hs.hole_repairs,
                lagging: hs.lagging,
                member: self.members.contains(seat),
                silent_ms: if Some(*seat) == local {
                    0
                } else {
                    hs.silent_ms(now_ms)
                },
                interrupted: hs.interrupted,
                catching_up: hs.catching_up,
                inputs_late_merged: hs.late_merged,
                hold_released: hs.hold_released,
                inputs_held: hs.held,
                late_stamp_suspect: hs.watch.suspect,
                late_stamp_flags: hs.watch.flagged,
                input_hold_p10_us: hs.watch.hold_p10_us,
                input_margin_p50_us: hs.watch.margin_p50_us,
            });
        }
        let host_record = self.role.voter().map(|v| self.quorum.record(v));
        let late_stamp_suspects = seats
            .iter()
            .filter(|s| s.late_stamp_suspect)
            .map(|s| s.seat)
            .collect();
        LockstepStatus {
            role: "host".to_owned(),
            tick: next_tick,
            delay_ticks: delay,
            delay_from,
            delay_target_ticks: self.control.last_target,
            delay_raises: self.control.raises,
            delay_lowers: self.control.lowers,
            calm_windows: self.control.calm_windows,
            gap_ticks: self.stamper.gap_ticks(),
            datagrams_sent: self.datagrams_sent,
            datagram_bytes_sent: self.datagram_bytes_sent,
            stream_bytes_sent: self.stream_bytes_sent,
            checkpoints_agreed: self.quorum.agreed,
            last_agreed_tick: self.quorum.last_agreed(),
            desyncs: self.quorum.desyncs,
            resyncs: self.quorum.resyncs,
            host_resyncs: host_record.map_or(0, |r| r.resyncs),
            ties: self.quorum.ties,
            fp_env_corrections: self.fp_env_corrections,
            late_stamp_suspects,
            seats,
            frames_repaired: 0,
            repair_ticks_max: 0,
            datagrams_lost: 0,
            single_losses: 0,
            single_loss_repair_ticks_max: 0,
            match_end_tick: self.ended_at,
            seats_timed_out: self.seats_timed_out,
            seats_left: self.seats_left,
            catchups: self.catchups,
            member: false,
            catching_up: false,
            desync_reports: self.desync_reports,
            last_desync: self.last_desync.clone(),
            rollback_window_ticks: self.config.mode.window_ticks(),
            ..LockstepStatus::default()
        }
    }
}

/// Folds a seat's late inputs into its input for the tick being built (spec 120 § 6.3): with the
/// game's rules, presses of every late input are kept and the newest analog values win; without
/// them, the seat's own input wins and the newest late input fills a tick it has none for. The
/// result is encoded and decoded again, so the host steps what the frame carries.
fn merge_late<I: NetCodec + Clone>(
    rules: Option<InputRules<I>>,
    late: Vec<(u64, Held<I>)>,
    current: Option<Held<I>>,
    max_input: usize,
) -> Option<Held<I>> {
    let Some(rules) = rules else {
        return current.or_else(|| late.into_iter().max_by_key(|(tick, _)| *tick).map(|(_, h)| h));
    };
    let mut late = late;
    late.sort_by_key(|(tick, _)| *tick);
    let mut acc: Option<I> = None;
    for value in late
        .iter()
        .map(|(_, h)| &h.value)
        .chain(current.as_ref().map(|h| &h.value))
    {
        acc = Some(match acc {
            None => value.clone(),
            Some(older) => {
                let mut next = value.clone();
                (rules.merge_late)(&older, &mut next);
                next
            }
        });
    }
    let merged = acc?;
    match merged.to_bytes() {
        Ok(bytes) if bytes.len() <= max_input => match I::from_bytes(&bytes) {
            Ok(value) => Some(Held { bytes, value }),
            Err(_) => current.or_else(|| late.pop().map(|(_, h)| h)),
        },
        _ => current.or_else(|| late.pop().map(|(_, h)| h)),
    }
}

/// The input the missing-input policy gives a seat without one this tick, from its delta base:
/// its last input under `Hold`, that with the presses cleared under `RepeatAnalog` (absent without
/// rules), nothing under `Empty` or before its first input.
pub(crate) fn held_input<I: NetCodec>(
    policy: MissingInputPolicy,
    rules: Option<InputRules<I>>,
    base: Option<&wire::SeatDelta>,
) -> Option<I> {
    let base = base.filter(|b| b.seen)?;
    match policy {
        MissingInputPolicy::Empty => None,
        MissingInputPolicy::Hold => I::from_bytes(&base.bytes).ok(),
        MissingInputPolicy::RepeatAnalog => {
            let rules = rules?;
            I::from_bytes(&base.bytes).ok().map(|v| (rules.clear_presses)(&v))
        }
    }
}

/// The frame a decoded body steps, as a seat hands it out: `state` is the delta base after it.
fn frame_of<G: NetGame>(
    raw: &RawFrame,
    state: &FrameState,
    config: &LockstepConfig,
    rules: Option<InputRules<G::Input>>,
) -> Result<LockstepFrame<G::Input>, LockstepError> {
    let mut inputs = Vec::with_capacity(state.seats.len());
    if raw.gap {
        inputs.extend(state.seats.keys().map(|seat| (*seat, SeatInput::Absent)));
    } else {
        for (seat, entry) in &raw.entries {
            inputs.push((
                *seat,
                match entry {
                    Some(bytes) => SeatInput::Present(G::Input::from_bytes(bytes)?),
                    None => held_input::<G::Input>(config.missing_input, rules, state.seats.get(seat))
                        .map_or(SeatInput::Absent, SeatInput::Held),
                },
            ));
        }
    }
    Ok(LockstepFrame {
        tick: raw.tick,
        inputs,
        delay: state.delay,
        delay_from: state.delay_from,
        membership: raw.membership.clone(),
        match_end: raw.match_end,
    })
}
