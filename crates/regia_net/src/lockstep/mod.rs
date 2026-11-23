// SPDX-License-Identifier: MPL-2.0
//! Lockstep mode (spec 120 § 7.1-7.5; ADR-8146, ADR-8212).
//!
//! For games whose whole simulation is deterministic and too large to replicate. One **host**
//! ([`LockstepHost`]: a listen-server player or a dedicated observer process) decides one
//! [`LockstepFrame`] per tick; every **seat** ([`LockstepSeat`]) sends its inputs stamped for a
//! later tick and steps its simulation only from frames, in order, each exactly once.
//!
//! The core is sans-IO and has no ECS: it turns bytes into frames and frames into bytes, with
//! millisecond time passed in. A transport carries two lanes ([`Lane`]): datagrams that may be
//! lost, duplicated or reordered, and a reliable ordered stream. [`net`] maps the lanes onto
//! `NetServer` / `NetClient` game messages (no framing change, the wire version stays as it is).
//!
//! What it does that a reliable frame channel does not:
//!
//! * **Frames travel as datagrams with redundancy** (§ 7.2). Each frame datagram carries the newest
//!   frame plus the frames before it that the seat has not acknowledged ([`FrameRedundancy`]), so
//!   one lost datagram is repaired by the next one, a tick later, instead of by a resend timer.
//!   Seats acknowledge the newest contiguous frame in their input datagrams. Per-seat inputs are delta-coded against the previous tick
//!   (one bit for an unchanged seat, a byte mask for a changed one). Frames that do not fit the
//!   datagram budget go on the stream, once.
//! * **The frame carries the bytes each seat sent.** The host never re-encodes an input: every
//!   participant, the host included, decodes the same bytes, so a lossy or sign-sensitive input
//!   codec (a `-0.0`, a NaN payload) cannot make two seats step different values.
//! * **Input delay control** (§ 7.3). Inputs are stamped `capture tick + delay`. The delay is
//!   raised at once (the gap ticks are listed empty for every seat) and lowered by one tick after
//!   five calm two-second windows; a change names the capture tick it starts at and rides in the
//!   frames, so every seat switches on the same tick. The target is the measured input lateness
//!   p99 at the host plus the p99 loss-run recovery of the link, not the round-trip time alone,
//!   bounded by [`LockstepConfig::max_delay_ticks`]. Every step of it is integer arithmetic.
//! * **Equal delay for a player host** ([`HostInput::Delayed`], the default): the host's own
//!   input is stamped like everyone else's.
//! * **Hash authority by quorum** (ADR-8146): seats report a state hash at checkpoint ticks and the
//!   host tallies them with its own. A strict majority decides; a seat or the host in the minority
//!   is resynced from the majority, and a tie never blames or removes anyone ([`DesyncVerdict`]).
//!
//! * **Sessions** (§ 7.4, ADR-8212). A silent seat never holds a frame past its deadline: it is
//!   listed absent, or with the input the game's [`MissingInputPolicy`] gives it
//!   ([`SeatInput::Held`]), and its inputs that arrive late are merged into the next frame built
//!   ([`InputRules`]), so a press is never lost or doubled. Joins, leaves (a seat asks with
//!   [`LockstepSeat::leave`]) and the match end ([`LockstepHost::end_match`]) are agreed on a
//!   frame's tick. Silence is judged by the session's [`TimeoutPolicy`]: an interruption notice,
//!   a start grace for loaders, removal only at the timeout. A seat that joins late or returns
//!   from an outage catches up from the game's state at a recent tick plus every frame since
//!   ([`LockstepHost::catch_up`]), steps them under a per-frame budget and joins on an agreed tick;
//!   a resume token ([`LockstepHost::resume_token`]) gives it back its seat id.
//!
//! * **The probe names the subsystem** (§ 7.6, ADR-8211): a simulation that hashes with
//!   `record_state` ([`crate::desync::DesyncHash`]) keeps 240 ticks of subsystem rows; after a
//!   verdict the host finds the first diverged tick and then compares the rows there
//!   ([`HostEvent::DesyncProbed`]).
//! * **Rollback lockstep** (§ 7.7, ADR-8361). Under [`LockstepMode::Rollback`] a seat does not
//!   wait for frames: a [`RollbackRunner`] predicts the inputs it lacks (each remote seat repeats
//!   its last input with the presses cleared), runs ahead of the newest frame by at most the
//!   rollback window with a `RollbackLog`, and replays from the first tick whose real inputs
//!   differ from the prediction when the frame arrives. The host holds a frame for a live seat's
//!   input for up to twice the window, so an input is waited for rather than dropped; the frames,
//!   and every state hash, are the strict match's.
//!
//! * **Host migration** (§ 10.5, ADR-8809): every seat keeps a frame cache; when the host quits the
//!   seats elect the best connected survivor with authenticated messages, and it resumes the match
//!   at the highest next tick ([`HostMigration`]).
//!
//! Not in this module: the desync bundle (§ 7.6).
//!
//! ```
//! use regia_net::lockstep::{HostRole, Lane, LockstepConfig, LockstepHost, LockstepSeat, SeatId};
//! use regia_net::{NetGame, WireError, WireReader, WireWriter};
//!
//! struct Game;
//! impl NetGame for Game {
//!     type Message = Vec<u8>;
//!     type Input = u8;
//!     type JoinData = ();
//!     const GAME_ID: &'static str = "doc.lockstep";
//!     const PROTOCOL_VERSION: u32 = 1;
//! }
//!
//! let config = LockstepConfig::default();
//! let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
//! let mut seat = LockstepSeat::<Game>::new(config, SeatId(0));
//! host.add_seat(SeatId(0));
//! host.start(0);
//! let mut stepped = Vec::new();
//! for now_ms in (0..200u64).step_by(17) {
//!     // The seat samples one input per capture tick and sends it.
//!     let _ = seat.submit_input(&7);
//!     for out in seat.outgoing(now_ms) {
//!         host.receive(SeatId(0), out.lane, &out.bytes, now_ms).expect("seat bytes");
//!     }
//!     // The host builds the due frames and sends them.
//!     while host.poll_frame(now_ms).is_some() {}
//!     for (to, out) in host.outgoing(now_ms) {
//!         assert_eq!(to, SeatId(0));
//!         seat.receive(out.lane, &out.bytes, now_ms).expect("host bytes");
//!     }
//!     // The seat steps only from frames, in order.
//!     while let Some(frame) = seat.next_frame() {
//!         stepped.push(frame.tick);
//!     }
//! }
//! assert!(stepped.len() > 8);
//! assert!(stepped.windows(2).all(|w| w[1] == w[0] + 1), "in order, each once");
//! # let _ = (Lane::Datagram, WireError::Utf8, WireReader::new(&[]), WireWriter::new());
//! ```

mod delay;
mod host;
mod loss;
mod migration;
pub mod net;
mod quorum;
pub mod replay;
mod rollback;
mod seat;
/// The lockstep server game type (ADR-8558). Its code is the host-side module
/// [`crate::lockstep_server`] (ADR-8656): it owns the observer thread and its step clock, which the
/// determinism scan keeps out of this directory, and drives the frame authority through its public
/// tick API. This path stays for callers.
pub use crate::lockstep_server as server;
mod stamp;
mod watch;
mod wire;

#[cfg(test)]
mod tests;
// Desync detection end to end (spec 120 § 7.6; ADR-8211).
#[cfg(test)]
mod desync_tests;
// Host migration end to end (spec 120 § 10.5; ADR-8809).
#[cfg(test)]
mod migration_tests;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use host::{HostEvent, HostRole, LockstepHost};
pub use migration::{
    HostMigration, MIGRATION_PORT_SPAN, MigrationAction, MigrationConfig, MigrationError, MigrationEvent,
    MigrationRoster, MigrationStatus, RTT_SHARE_MS, RosterSeat, RttMatrix, listen_ports, rank_survivors,
};
pub use quorum::{DesyncVerdict, TiePolicy, VerdictKind, Voter};
pub use rollback::{RollbackAdvance, RollbackGame, RollbackRunner};
pub use seat::{LockstepSeat, SeatEvent};
pub use stamp::Stamp;

use crate::bits::DecodeError;
pub use crate::session::TimeoutPolicy;
use crate::wire::WireError;

/// A seat of a lockstep match: a slot a player's inputs are listed under, independent of the
/// connection that carries them. Frames list seats in ascending order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SeatId(pub u16);

impl std::fmt::Display for SeatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seat{}", self.0)
    }
}

/// One seat's input for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatInput<I> {
    /// The input the seat sent for the tick, decoded from the bytes it sent.
    Present(I),
    /// No input for the tick: it did not arrive by the frame's deadline, or the tick is a gap
    /// tick of a delay raise. The game's missing-input policy applies.
    Absent,
    /// No input arrived for the tick; the match's [`MissingInputPolicy`] supplies this one: the
    /// seat's last input ([`MissingInputPolicy::Hold`]) or it with its presses cleared
    /// ([`MissingInputPolicy::RepeatAnalog`]). Every participant derives the same value from the
    /// frame chain; the wire carries the seat as absent.
    Held(I),
}

impl<I> SeatInput<I> {
    /// The input to step with: the one the seat sent, or the one the missing-input policy holds.
    pub fn as_ref(&self) -> Option<&I> {
        match self {
            Self::Present(input) | Self::Held(input) => Some(input),
            Self::Absent => None,
        }
    }

    /// Whether the seat's own input for the tick is in the frame (not absent, not held).
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }
}

/// What a seat without an input for a tick steps with (spec 120 § 7.4). A match setting: every
/// participant must use the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingInputPolicy {
    /// Nothing: the seat is listed [`SeatInput::Absent`].
    #[default]
    Empty,
    /// The seat's last input with its presses cleared ([`InputRules::clear_presses`]), listed
    /// [`SeatInput::Held`]. Without input rules the seat is listed absent.
    RepeatAnalog,
    /// The seat's last input as it was, listed [`SeatInput::Held`].
    Hold,
}

/// The game's rules for inputs that miss their tick (spec 120 § 6.3 and § 7.4). Every participant
/// sets the same rules: they are the game's code, like its simulation.
pub struct InputRules<I> {
    /// The input with every one-shot press cleared and the analog values kept, for
    /// [`MissingInputPolicy::RepeatAnalog`].
    pub clear_presses: fn(&I) -> I,
    /// Folds `late`, an input that arrived after its tick was built, into `next`, the seat's
    /// input for the next tick built: presses of both are kept, `next`'s analog values win. The
    /// host applies it; the frame carries the result.
    pub merge_late: fn(late: &I, next: &mut I),
}

impl<I> Clone for InputRules<I> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I> Copy for InputRules<I> {}

impl<I> std::fmt::Debug for InputRules<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputRules").finish_non_exhaustive()
    }
}

/// Whether a seat joins or leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipKind {
    /// The seat is listed from this tick on.
    Join,
    /// The seat is no longer listed from this tick on.
    Leave,
}

/// A seat that joins or leaves at a frame's tick (agreed-tick membership).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MembershipChange {
    /// The seat.
    pub seat: SeatId,
    /// Join or leave.
    pub kind: MembershipKind,
}

/// One lockstep tick as the host decided it (spec 120 § 7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockstepFrame<I> {
    /// The tick this frame steps.
    pub tick: u64,
    /// Every seat's input for the tick, sorted by seat; a seat absent this tick is listed with
    /// [`SeatInput::Absent`].
    pub inputs: Vec<(SeatId, SeatInput<I>)>,
    /// The input delay in force from `delay_from` on (agreed-tick change).
    pub delay: u16,
    /// First capture tick the `delay` above applies to: an input sampled while the next tick to
    /// step is `c >= delay_from` is stamped `c + delay`.
    pub delay_from: u64,
    /// Seats that join or leave at this tick (agreed-tick membership).
    pub membership: Vec<MembershipChange>,
    /// The match ends with this tick (agreed-tick match end, [`LockstepHost::end_match`]): no
    /// frame follows it.
    pub match_end: bool,
}

/// Which lane a lockstep payload travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// May be lost, duplicated or reordered; never resent by the transport. The module sequences
    /// and repairs these itself.
    Datagram,
    /// Delivered exactly once, in order (the lockstep stream).
    Stream,
}

/// A lockstep payload to hand to the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The lane to send it on.
    pub lane: Lane,
    /// The payload.
    pub bytes: Vec<u8>,
}

/// How a player host's own input is stamped (spec 120 § 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostInput {
    /// Stamped `capture tick + delay` like every seat's: no structural host advantage.
    #[default]
    Delayed,
    /// Goes into the next frame built.
    Immediate,
}

/// Who the frame delay follows (spec 120 § 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelayPolicy {
    /// Every seat uses the frame delay, which follows the seat that needs the most.
    #[default]
    Shared,
    /// The frame delay follows the median seat; a seat whose input margin is short stamps its
    /// inputs further ahead on its own, steered by the host's `LinkFeedback`.
    PerSeat,
}

/// How many frames a frames datagram repeats (spec 120 § 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRedundancy {
    /// Every datagram carries the newest frame and the unacknowledged one before it, so one lost
    /// datagram is always repaired by the next; every second datagram reaches back over the loss
    /// run the seat's link produces (the fitted p99 sequence jump, and at least the longest one
    /// of the last minute) plus two, so a longer run is repaired by the first or the second
    /// datagram after it. A link that only loses single datagrams never gets the deeper
    /// datagram. A seat that finds a hole says so in its inputs datagram and the host then sends
    /// every unacknowledged frame until the hole is closed. Against [`Self::Adaptive`] this
    /// halves what a lossy link's redundancy costs the host, for one tick more on the runs
    /// longer than one datagram (ADR-8209).
    #[default]
    Interleaved,
    /// The newest frame plus the unacknowledged frames before it, back to one more than the
    /// sequence jump the seat's link produces, in every datagram (two frames at least): any run
    /// up to that jump is repaired by the next datagram. On a clean link with a long round trip
    /// this sends each frame twice instead of once per tick of round trip.
    Adaptive,
    /// The newest frame plus every frame the seat has not acknowledged, always (the cost grows
    /// with the round-trip time).
    AllUnacked,
}

/// The most ticks a rollback seat runs ahead of its newest frame (spec 120 § 7.7: a rollback window
/// of up to 8 ticks).
pub const MAX_ROLLBACK_TICKS: u16 = 8;

/// How seats step (spec 120 § 7.1 and § 7.7). A match setting: every participant uses the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockstepMode {
    /// A seat steps only from frames: the input delay has to cover the round trip.
    #[default]
    Strict,
    /// A seat runs ahead of its newest frame by up to `max_rollback_ticks` with predicted inputs
    /// and rolls back when a frame's real inputs differ ([`RollbackRunner`]); the host holds a
    /// frame for a live seat's input for up to twice that many ticks past its time (the other
    /// seats predict through the first window and wait at its edge for the rest), then lists the
    /// seat with its held input and merges the late one. For small action matches
    /// (2-8 seats) with an input delay of 1-3 ticks ([`LockstepConfig::rollback`]).
    Rollback {
        /// The rollback window, ticks (1..=[`MAX_ROLLBACK_TICKS`]).
        max_rollback_ticks: u16,
    },
}

impl LockstepMode {
    /// How many ticks a seat may run ahead of its newest frame: 0 in strict mode, the rollback
    /// window otherwise.
    pub fn window_ticks(self) -> u16 {
        match self {
            Self::Strict => 0,
            Self::Rollback { max_rollback_ticks } => max_rollback_ticks,
        }
    }

    fn sanitized(self) -> Self {
        match self {
            Self::Strict => Self::Strict,
            Self::Rollback { max_rollback_ticks } => Self::Rollback {
                max_rollback_ticks: max_rollback_ticks.clamp(1, MAX_ROLLBACK_TICKS),
            },
        }
    }
}

/// Lockstep settings. Every participant of a match must use the same values: they are match
/// settings, like the tick rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LockstepConfig {
    /// Ticks per second (1..=1000).
    pub tick_rate_hz: u32,
    /// The input delay the match starts with, ticks.
    pub initial_delay_ticks: u16,
    /// The smallest input delay the controller lowers to, ticks.
    pub min_delay_ticks: u16,
    /// The largest input delay, ticks (15 at 60 Hz: 250 ms).
    pub max_delay_ticks: u16,
    /// How a player host's own input is stamped.
    pub host_input: HostInput,
    /// Who the frame delay follows.
    pub delay_policy: DelayPolicy,
    /// Length of one delay window, milliseconds.
    pub delay_window_ms: u64,
    /// Calm windows in a row before the delay drops by one tick.
    pub calm_windows_to_lower: u32,
    /// How long the host holds a due frame for a live seat's missing input, milliseconds
    /// (`0`: never; the frame is built at its time).
    pub late_grace_ms: u64,
    /// How many frames a frames datagram repeats.
    pub redundancy: FrameRedundancy,
    /// Largest lockstep datagram payload, bytes; frames that do not fit go on the stream.
    pub datagram_budget_bytes: usize,
    /// Largest encoded input a seat may send, bytes (each input is sent on to every seat).
    pub max_input_bytes: usize,
    /// Future ticks of input the host buffers per seat.
    pub input_buffer_ticks: usize,
    /// Frames both ends keep for a resync or a late join, ticks.
    pub frame_log_ticks: usize,
    /// A state hash is voted on every this many ticks (`0`: never).
    pub hash_every: u32,
    /// Ticks after a checkpoint at which its tally closes without the missing votes. At most
    /// 24 checkpoint intervals (`hash_every`): the quorum keeps a bounded number of checkpoints
    /// open, and a deadline longer than that would leave new checkpoints unopened.
    pub vote_deadline_ticks: u32,
    /// Ask voters outside the majority to replay from the last agreed checkpoint and vote again
    /// before a state transfer is ordered.
    pub self_check: bool,
    /// State transfers a seat may need before it is removed with `desync`; for the host, before
    /// `HostEvent::HostFault`.
    pub max_resyncs: u32,
    /// What decides a tally without a strict majority.
    pub tie_policy: TiePolicy,
    /// Ties resolved by `tie_policy` before the match is reported unresolved.
    pub max_tie_resyncs: u32,
    /// Most ticks a seat stamps ahead of the frame delay under [`DelayPolicy::PerSeat`].
    pub max_extra_lead_ticks: u16,
    /// What a seat without an input for a tick steps with.
    pub missing_input: MissingInputPolicy,
    /// Silence policy for seats (spec 120 § 10.1): an interruption notice after `interrupt_ms`,
    /// removal after `timeout_ms`, never before `start_grace_ms` from the seat's admission for a
    /// seat that has not been heard yet (a loader).
    pub timeout: TimeoutPolicy,
    /// Frames a catching-up seat hands out per application frame at most (the CPU budget of a
    /// catch-up, spec 120 § 7.4); between two calls of `LockstepSeat::outgoing`.
    pub catchup_steps_per_frame: u32,
    /// How long a seat removed for silence may come back with its resume token, milliseconds.
    pub resume_window_ms: u64,
    /// Strict lockstep or rollback lockstep (spec 120 § 7.7).
    pub mode: LockstepMode,
    /// Under rollback: stop holding due frames for a seat whose median input arrives more than the
    /// window late, so one slow link does not make the other seats wait (KI-NT-038, ADR-8855).
    /// `false` keeps the pure two-window hold for every live seat (every seat waits for the slowest,
    /// as a 1v1 game may prefer). No effect in strict mode.
    pub release_slow_seat_hold: bool,
}

impl Default for LockstepConfig {
    fn default() -> Self {
        Self {
            tick_rate_hz: 60,
            initial_delay_ticks: 4,
            min_delay_ticks: 1,
            max_delay_ticks: 15,
            host_input: HostInput::Delayed,
            delay_policy: DelayPolicy::Shared,
            delay_window_ms: 2_000,
            calm_windows_to_lower: 5,
            late_grace_ms: 0,
            redundancy: FrameRedundancy::Interleaved,
            datagram_budget_bytes: 1_000,
            max_input_bytes: 1_024,
            input_buffer_ticks: 64,
            frame_log_ticks: 600,
            hash_every: 30,
            vote_deadline_ticks: 120,
            self_check: true,
            max_resyncs: 2,
            tie_policy: TiePolicy::HostGroup,
            max_tie_resyncs: 2,
            max_extra_lead_ticks: 8,
            missing_input: MissingInputPolicy::Empty,
            timeout: TimeoutPolicy::default(),
            catchup_steps_per_frame: 8,
            resume_window_ms: 120_000,
            mode: LockstepMode::Strict,
            release_slow_seat_hold: true,
        }
    }
}

impl LockstepConfig {
    /// The defaults at `tick_rate_hz`, with the largest delay kept at 250 ms (15 ticks at 60 Hz).
    pub fn at_rate(tick_rate_hz: u32) -> Self {
        let hz = tick_rate_hz.clamp(1, 1000);
        let max = (15 * hz).div_ceil(60).clamp(1, u32::from(u16::MAX)) as u16;
        Self {
            tick_rate_hz: hz,
            max_delay_ticks: max,
            initial_delay_ticks: 4.min(max),
            ..Self::default()
        }
        .sanitized()
    }

    /// Rollback lockstep at `tick_rate_hz` (spec 120 § 7.7): an input delay of 2 ticks that the
    /// controller keeps between 1 and 3, a rollback window of [`MAX_ROLLBACK_TICKS`], and missing
    /// inputs held with their presses cleared ([`MissingInputPolicy::RepeatAnalog`], which needs
    /// the game's [`InputRules`]).
    pub fn rollback(tick_rate_hz: u32) -> Self {
        Self {
            min_delay_ticks: 1,
            initial_delay_ticks: 2,
            max_delay_ticks: 3,
            missing_input: MissingInputPolicy::RepeatAnalog,
            mode: LockstepMode::Rollback {
                max_rollback_ticks: MAX_ROLLBACK_TICKS,
            },
            ..Self::at_rate(tick_rate_hz)
        }
        .sanitized()
    }

    /// The settings with every field inside its range, so the core never divides by zero or
    /// buffers without bound whatever a file said.
    pub fn sanitized(mut self) -> Self {
        self.tick_rate_hz = self.tick_rate_hz.clamp(1, 1000);
        self.max_delay_ticks = self.max_delay_ticks.clamp(1, 1_000);
        self.min_delay_ticks = self.min_delay_ticks.min(self.max_delay_ticks);
        self.initial_delay_ticks = self
            .initial_delay_ticks
            .clamp(self.min_delay_ticks, self.max_delay_ticks);
        self.delay_window_ms = self.delay_window_ms.clamp(100, 60_000);
        self.calm_windows_to_lower = self.calm_windows_to_lower.max(1);
        self.late_grace_ms = self.late_grace_ms.min(self.tick_ms_ceil());
        self.datagram_budget_bytes = self.datagram_budget_bytes.clamp(128, 60_000);
        self.max_input_bytes = self.max_input_bytes.clamp(1, 60_000);
        self.mode = self.mode.sanitized();
        // A rollback seat stamps up to its window past the frames it has.
        self.input_buffer_ticks = self.input_buffer_ticks.clamp(
            usize::from(self.max_delay_ticks) + usize::from(self.mode.window_ticks()) + 2,
            4_096,
        );
        self.frame_log_ticks = self.frame_log_ticks.clamp(64, 1 << 20);
        self.vote_deadline_ticks = self.vote_deadline_ticks.max(1);
        if self.hash_every > 0 {
            // 24 intervals, twice with a self-check round, stay under the quorum's open checkpoints.
            self.vote_deadline_ticks = self.vote_deadline_ticks.min(self.hash_every.saturating_mul(24));
        }
        self.max_extra_lead_ticks = self.max_extra_lead_ticks.min(self.max_delay_ticks);
        self.timeout = self.timeout.sanitized();
        // At least two frames per application frame: a catch-up has to outrun the live edge.
        self.catchup_steps_per_frame = self.catchup_steps_per_frame.clamp(2, 10_000);
        self.resume_window_ms = self.resume_window_ms.min(3_600_000);
        self
    }

    /// Length of one tick, microseconds.
    pub fn tick_us(&self) -> u64 {
        1_000_000 / u64::from(self.tick_rate_hz.clamp(1, 1000))
    }

    fn tick_ms_ceil(&self) -> u64 {
        self.tick_us().div_ceil(1000)
    }
}

/// Why a lockstep payload or call was refused. [`LockstepError::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LockstepError {
    /// The bytes do not decode as a lockstep payload of this lane.
    #[error("malformed lockstep payload: {0}")]
    Malformed(&'static str),
    /// A bit field of the payload did not decode.
    #[error("malformed lockstep payload: {0}")]
    Bits(#[from] DecodeError),
    /// An input inside the payload did not decode with the game's codec.
    #[error("lockstep input does not decode: {0}")]
    Input(#[from] WireError),
    /// The seat is not part of the match.
    #[error("{0} is not a seat of this match")]
    UnknownSeat(SeatId),
    /// An input is longer than [`LockstepConfig::max_input_bytes`].
    #[error("input of {len} bytes exceeds the {max}-byte limit")]
    InputTooLarge {
        /// Encoded length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The host has not been started.
    #[error("the lockstep host has not started")]
    NotStarted,
    /// The host has no seat of its own (it is not a listen host).
    #[error("this lockstep host has no local seat")]
    NoLocalSeat,
    /// An input was already submitted for the open capture tick; the next one opens with the next
    /// frame stepped.
    #[error("an input was already submitted for this capture tick")]
    CaptureClosed,
    /// A catch-up cannot start from the state at `state_tick`: the frame log does not hold every
    /// frame after it (spec 120 § 7.4).
    #[error(
        "no catch-up from the state at tick {state_tick}: the frame log holds the frames {first_logged}..{next_build}"
    )]
    CatchupGap {
        /// The tick of the offered state.
        state_tick: u64,
        /// The oldest frame the host still holds.
        first_logged: u64,
        /// The next frame the host builds.
        next_build: u64,
    },
    /// The resume token names no seat of this match, or the seat's resume window has passed.
    #[error("the resume token names no seat that may return")]
    ResumeRefused,
    /// The match has ended: no frame is built and no input is taken after the end frame.
    #[error("the lockstep match has ended")]
    MatchEnded,
}

impl LockstepError {
    /// The agent-protocol error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) | Self::Bits(_) => "lockstep_malformed",
            Self::Input(_) => "lockstep_input_invalid",
            Self::UnknownSeat(_) => "lockstep_unknown_seat",
            Self::InputTooLarge { .. } => "lockstep_input_too_large",
            Self::NotStarted => "lockstep_not_started",
            Self::NoLocalSeat => "lockstep_no_local_seat",
            Self::CaptureClosed => "lockstep_capture_closed",
            Self::CatchupGap { .. } => "lockstep_catchup_gap",
            Self::ResumeRefused => "lockstep_resume_refused",
            Self::MatchEnded => "lockstep_match_ended",
        }
    }

    /// True for a payload the seat itself got wrong (ADR-8734): bytes that do not decode as a
    /// lockstep payload, a field or input that does not decode, or an input over the limit. An
    /// honest seat never sends one (the transport delivers whole, authenticated messages), so a
    /// host scores these ([`net::SeatScores`]). The others (an unknown seat, a closed capture, a
    /// match not started or ended, a refused resume) are state, not malformation.
    pub fn is_malformation(&self) -> bool {
        matches!(
            self,
            Self::Malformed(_) | Self::Bits(_) | Self::Input(_) | Self::InputTooLarge { .. }
        )
    }
}

/// One seat of [`LockstepStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SeatLockstepStatus {
    /// The seat.
    pub seat: u16,
    /// Frames built that the seat has not acknowledged, ticks.
    pub frame_lag_ticks: u64,
    /// The largest `frame_lag_ticks` seen.
    pub frame_lag_max_ticks: u64,
    /// Ticks built without this seat's input (gap ticks of a delay raise are not counted).
    pub inputs_missing: u64,
    /// Inputs that arrived after their frame was built.
    pub inputs_late: u64,
    /// Inputs refused for being at or past the host's input buffer.
    pub inputs_too_far_ahead: u64,
    /// The seat's input lateness p99 over the last window, microseconds; `None` without samples.
    pub lateness_p99_us: Option<i64>,
    /// The p99 loss run of the link, datagrams (the larger of the two directions).
    pub loss_run_p99: u32,
    /// The delay this seat alone would need, ticks; `None` without samples.
    pub delay_need_ticks: Option<u16>,
    /// Ticks the seat stamps ahead of the frame delay ([`DelayPolicy::PerSeat`]).
    pub extra_lead_ticks: u16,
    /// State transfers ordered for the seat.
    pub resyncs: u32,
    /// Times the seat repaired itself by replaying from the last agreed checkpoint.
    pub self_heals: u32,
    /// The seat's hash is being ignored until it votes with the majority again.
    pub resyncing: bool,
    /// Frames handed to the stream for this seat because they did not fit a datagram.
    pub stream_frames: u64,
    /// Times the seat reported a hole in its frames and got every unacknowledged frame again.
    pub hole_repairs: u64,
    /// The seat fell further behind than the frame log; it needs a state transfer.
    pub lagging: bool,
    /// The seat is listed in the frames (a catching-up seat is not, until it joins).
    pub member: bool,
    /// Milliseconds since the seat was last heard (since its admission when never heard).
    pub silent_ms: u64,
    /// The seat has been silent past the interruption threshold.
    pub interrupted: bool,
    /// The seat is catching up from a state transfer and has not joined or resumed yet.
    pub catching_up: bool,
    /// Late inputs merged into a later frame.
    pub inputs_late_merged: u64,
    /// Ticks the missing-input policy supplied the seat's input ([`SeatInput::Held`]).
    pub inputs_held: u64,
    /// The seat's inputs are held back (late stamping, ADR-8209): over the last ten seconds nine
    /// in ten of them arrived `input_hold_p10_us` or more after the seat had acknowledged the
    /// frame that opened their capture, at least two ticks, and its median margin before the
    /// deadline is no larger than that hold. A flag to act on by hand: nobody is removed for it.
    #[serde(default)]
    pub late_stamp_suspect: bool,
    /// Times the seat became suspect.
    #[serde(default)]
    pub late_stamp_flags: u32,
    /// The hold nine in ten of the seat's inputs reached over the last ten seconds, microseconds:
    /// from the datagram that first acknowledged the frame that opened the capture to the
    /// input's first arrival, plus the time its acknowledgements trail the transport's round
    /// trip when the application feeds that in. Zero for an honest seat on any link; `None`
    /// without enough samples.
    #[serde(default)]
    pub input_hold_p10_us: Option<u64>,
    /// The seat's median margin over the same window, microseconds: how long before its frame
    /// was built an input arrived. `None` without enough samples.
    #[serde(default)]
    pub input_margin_p50_us: Option<i64>,
    /// Under rollback: the seat's median input arrives later than the rollback window, so the
    /// host builds due frames without holding them for it; its late inputs are merged late
    /// (KI-NT-038, ADR-8855).
    #[serde(default)]
    pub hold_released: bool,
}

/// What `net.lockstep` reports (spec 120 § 13.3): the delay, frame lag per seat and resyncs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LockstepStatus {
    /// `host` or `seat`.
    pub role: String,
    /// The next tick to build (host) or to step (seat).
    pub tick: u64,
    /// The input delay in force for the next capture tick, ticks.
    pub delay_ticks: u16,
    /// First capture tick the delay applies to.
    pub delay_from: u64,
    /// The delay the measurements ask for, ticks (host only).
    pub delay_target_ticks: Option<u16>,
    /// Delay raises so far.
    pub delay_raises: u64,
    /// Delay lowers so far.
    pub delay_lowers: u64,
    /// Calm windows counted toward the next lower.
    pub calm_windows: u32,
    /// Gap ticks listed empty for every seat by delay raises (and the start).
    pub gap_ticks: u64,
    /// Lockstep datagrams sent.
    pub datagrams_sent: u64,
    /// Bytes of lockstep datagram payloads sent.
    pub datagram_bytes_sent: u64,
    /// Bytes of lockstep stream payloads sent.
    pub stream_bytes_sent: u64,
    /// Checkpoints every voter agreed on.
    pub checkpoints_agreed: u64,
    /// The last tick every voter (or the majority) agreed on.
    pub last_agreed_tick: Option<u64>,
    /// Desync verdicts so far.
    pub desyncs: u64,
    /// State transfers ordered so far (seats and host).
    pub resyncs: u64,
    /// State transfers ordered for the host itself.
    pub host_resyncs: u32,
    /// Tallies without a strict majority.
    pub ties: u32,
    /// Times the floating-point environment of the stepping thread was put back to the default
    /// before a frame was handed out.
    pub fp_env_corrections: u64,
    /// The seats whose inputs are held back right now (`seats[].late_stamp_suspect`; host only).
    #[serde(default)]
    pub late_stamp_suspects: Vec<u16>,
    /// Per-seat rows (host only).
    pub seats: Vec<SeatLockstepStatus>,
    /// Frames this seat got from a datagram in which they were not the newest (repaired by
    /// redundancy; seat only).
    pub frames_repaired: u64,
    /// The longest repair, in ticks after the frame's own datagram (seat only).
    pub repair_ticks_max: u64,
    /// Frame datagrams this seat's sequence shows lost (seat only).
    pub datagrams_lost: u64,
    /// Times exactly one frames datagram was missing before the one that arrived, with the seat's
    /// frames whole until then (seat only).
    pub single_losses: u64,
    /// The longest repair after such a single loss, in ticks after the frame's own datagram:
    /// the stall one lost datagram costs (seat only).
    pub single_loss_repair_ticks_max: u64,
    /// The tick of the frame that ended the match.
    pub match_end_tick: Option<u64>,
    /// Seats removed for silence past the timeout (host only).
    pub seats_timed_out: u64,
    /// Seats that left on their own request (host only).
    pub seats_left: u64,
    /// Catch-ups sent (host) or taken (seat).
    pub catchups: u64,
    /// The seat is listed in the frames (seat only).
    pub member: bool,
    /// The seat is catching up: frames received and not stepped yet beyond the live edge (seat
    /// only).
    pub catching_up: bool,
    /// Probe reports so far: diverged voters whose first diverged subsystem was looked for
    /// (host only; spec 120 § 7.6).
    #[serde(default)]
    pub desync_reports: u64,
    /// The newest probe report: the diverged voter, the first diverged tick and subsystem, the
    /// last agreed tick (host only).
    #[serde(default)]
    pub last_desync: Option<crate::desync::DesyncReport>,
    /// The rollback window, ticks; 0 in strict mode ([`LockstepMode`]).
    #[serde(default)]
    pub rollback_window_ticks: u16,
    /// Rollbacks: frames whose real inputs differed from the prediction, each replayed from the
    /// first such tick (seat only, rollback mode).
    #[serde(default)]
    pub rollbacks: u64,
    /// The deepest rollback, ticks replayed (never more than the window; seat only).
    #[serde(default)]
    pub rollback_ticks_max: u64,
    /// Ticks replayed by rollbacks, in total (seat only).
    #[serde(default)]
    pub rollback_ticks_replayed: u64,
    /// Ticks stepped ahead of the newest frame with predicted inputs (seat only).
    #[serde(default)]
    pub ticks_predicted: u64,
    /// Predicted ticks a frame confirmed without a rollback (seat only).
    #[serde(default)]
    pub predictions_confirmed: u64,
    /// Application frames in which the seat's clock was past the window and it waited for a
    /// frame instead of predicting further (seat only).
    #[serde(default)]
    pub window_stalls: u64,
    /// Ticks the seat is ahead of its newest frame right now (seat only).
    #[serde(default)]
    pub ahead_ticks: u64,
    /// The fewest and the most ticks between sampling a local input and stepping it, over the
    /// ticks the seat predicted: the input delay the player feels (seat only).
    #[serde(default)]
    pub local_input_lag_ticks: Option<(u64, u64)>,
    /// Ticks the seat's clock runs ahead of its first frame's arrival (negative: behind), as the
    /// clock sync steered it from the host's input margins (seat only, rollback mode).
    #[serde(default)]
    pub clock_lead_ticks: i64,
    /// Host migration (spec 120 § 10.5): the election this participant is in, the migrations so
    /// far and refused messages; set by [`HostMigration::annotate`].
    #[serde(default)]
    pub migration: Option<MigrationStatus>,
}
