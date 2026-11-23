// SPDX-License-Identifier: MPL-2.0
//! Input logs, offline replay and catch-up (DC-3.7b, DC-3.5b).
//!
//! A lockstep sim is a function of its state and its inputs. Keep the inputs of every tick, and a
//! snapshot ([`crate::state`]) from where they start, and any run can be stepped again, headless, on
//! another machine, to the same bits. That is what this module gives:
//!
//! - [`TickInputs`]: every seat's input for one tick, in ascending seat order, `None` for a seat with
//!   no input that tick. Its shape follows the lockstep frame of the net layer (spec 120 § 7.1: one
//!   `(seat, present or absent)` per member, sorted by seat), so a seat feeds received frames straight
//!   in; the net crate converts, this crate does not depend on it.
//! - [`DetInputSim`]: a sim that steps from inputs, saves itself ([`DetState`]) and names its systems
//!   ([`SystemFold`]). [`advance`] is **the** step: the live loop, [`replay`], [`CatchUp`] and the
//!   golden adapter all call it, so a replay runs exactly the code the match ran (there is no
//!   replay-only branch to drift).
//! - [`InputLog`]: the ticks with their inputs and, optionally, the hashes recorded after each,
//!   framed with the det-state codec (kind `InputLog`, version [`INPUT_LOG_VERSION`]) and read by a
//!   bounded reader. A log may carry the snapshot it starts from.
//! - [`replay`]: steps a sim restored from the snapshot through the log and compares every recomputed
//!   hash with the recorded one; the [`ReplayReport`] names the first diverging tick, the system
//!   ([`crate::hash::diverged_systems`]) and the last tick that agreed.
//! - [`CatchUp`]: a late joiner or a resynced seat steps queued frames at most `budget` ticks per call
//!   (spec 120 § 7.4: "faster than real time, bounded by a CPU budget per frame"). The budget is a tick
//!   count, so the result is deterministic; the host decides how many calls a frame makes.
//! - [`DesyncReplay`]: the object-safe hook a desync tool registers per sim ([`ReplayHook`] wraps any
//!   [`DetInputSim`]), and [`DesyncReplayRegistry`] to find it by the sim's tag.
//!
//! Seats are walked in sorted order everywhere (a sorted `Vec`, never a hash map: spec 120 § 7.6
//! records an offline replay that walked a hash map and disagreed with the match it replayed).
//!
//! ```
//! use regia_detsim::hash::{Mix, SystemFold, Systems};
//! use regia_detsim::replay::{self, DetInputSim, InputLog, TickInputs};
//! use regia_detsim::state::{self, DetReader, DetState, DetStateError, DetWriter};
//!
//! /// Each seat pushes a counter by its input.
//! #[derive(Default)]
//! struct Tally { sums: [i64; 2] }
//!
//! impl DetState for Tally {
//!     const STATE_TAG: [u8; 8] = *b"tally\0\0\0";
//!     const STATE_VERSION: u32 = 1;
//!     fn save(&self, w: &mut DetWriter) { w.i64(self.sums[0]); w.i64(self.sums[1]); }
//!     fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
//!         Ok(Tally { sums: [r.i64()?, r.i64()?] })
//!     }
//! }
//! impl SystemFold<2> for Tally {
//!     const SYSTEMS: Systems<2> = Systems::new(["left", "right"]);
//!     fn fold_systems(&self, mix: &mut [Mix; 2]) {
//!         mix[0].fold(&self.sums[0]);
//!         mix[1].fold(&self.sums[1]);
//!     }
//! }
//! impl DetInputSim<2> for Tally {
//!     type Input = i32;
//!     fn step(&mut self, _tick: u64, inputs: &TickInputs<i32>) {
//!         for entry in inputs.seats() {
//!             if let (Some(v), Some(sum)) = (entry.input, self.sums.get_mut(usize::from(entry.seat))) {
//!                 *sum += i64::from(v);
//!             }
//!         }
//!     }
//! }
//!
//! // Live: snapshot at the start, then record every tick through the one step path.
//! let mut live = Tally::default();
//! let start = state::snapshot(&live, 0).unwrap();
//! let mut log = InputLog::for_sim::<Tally, 2>(0);
//! log.set_start_snapshot(start).unwrap();
//! for tick in 0..10u64 {
//!     let inputs = TickInputs::from_sorted(tick, [(0, Some(1)), (1, (tick % 3 == 0).then_some(-2))]).unwrap();
//!     replay::step_recorded(&mut live, inputs, &mut log, true).unwrap();
//! }
//!
//! // Offline: the bytes alone reproduce the run.
//! let bytes = log.encode().unwrap();
//! let report = replay::replay_bytes::<Tally, 2>(&bytes, None, None, Default::default()).unwrap();
//! assert!(report.passed());
//! assert_eq!((report.ticks_run, report.hashes_compared), (10, 10));
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::hash::{self, Divergence, SystemFold, SystemHashes, TraceRow};
use crate::state::{self, DetReadLimits, DetReader, DetState, DetStateError, DetWriter, SnapshotKind};

/// Tag of an input log frame (the frame's `STATE_TAG` slot).
pub const INPUT_LOG_TAG: [u8; 8] = *b"inputlog";

/// Version of the input log body. The engine's to bump; another version is `bad_version`.
pub const INPUT_LOG_VERSION: u32 = 1;

/// Most seats one tick may list (the lockstep frame's own cap). More is `length_limit`.
pub const MAX_SEATS_PER_TICK: usize = 4096;

/// Seat number, as the lockstep frame carries it (`SeatId(u16)` in the net layer).
pub type Seat = u16;

/// One seat's entry in a tick: its input, or `None` (no input arrived by the frame's deadline, or a
/// gap tick). The game's missing-input policy applies to `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatEntry<I> {
    /// The seat.
    pub seat: Seat,
    /// Its input this tick.
    pub input: Option<I>,
}

/// Every listed seat's input for one tick, in strictly ascending seat order.
///
/// The order is checked on construction ([`TickInputs::from_sorted`], [`TickInputs::push`]) and on
/// decode, so a step that walks [`TickInputs::seats`] walks the same order on every peer and in every
/// replay. A seat that is not a member this tick is simply not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickInputs<I> {
    tick: u64,
    seats: Vec<SeatEntry<I>>,
}

impl<I> TickInputs<I> {
    /// No seat listed (a tick before anyone joined).
    pub const fn new(tick: u64) -> Self {
        TickInputs {
            tick,
            seats: Vec::new(),
        }
    }

    /// From `(seat, input)` pairs that must already be in strictly ascending seat order (the order
    /// a lockstep frame carries). A repeated or out-of-order seat is `seat_order`; more than
    /// [`MAX_SEATS_PER_TICK`] seats is `length_limit`.
    pub fn from_sorted(
        tick: u64,
        seats: impl IntoIterator<Item = (Seat, Option<I>)>,
    ) -> Result<Self, ReplayError> {
        let mut out = TickInputs::new(tick);
        for (seat, input) in seats {
            out.push(seat, input)?;
        }
        Ok(out)
    }

    /// Append a seat; it must come after every seat already listed (`seat_order`).
    pub fn push(&mut self, seat: Seat, input: Option<I>) -> Result<(), ReplayError> {
        if let Some(last) = self.seats.last()
            && last.seat >= seat
        {
            return Err(ReplayError::SeatOrder {
                tick: self.tick,
                seat,
            });
        }
        if self.seats.len() >= MAX_SEATS_PER_TICK {
            return Err(ReplayError::State(DetStateError::LengthLimit {
                len: self.seats.len() as u64 + 1,
                limit: MAX_SEATS_PER_TICK as u64,
            }));
        }
        self.seats.push(SeatEntry { seat, input });
        Ok(())
    }

    /// The tick these inputs step.
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// The listed seats, ascending.
    pub fn seats(&self) -> &[SeatEntry<I>] {
        &self.seats
    }

    /// `seat`'s input, if the seat is listed and has one this tick.
    pub fn get(&self, seat: Seat) -> Option<&I> {
        let i = self.seats.binary_search_by_key(&seat, |e| e.seat).ok()?;
        self.seats[i].input.as_ref()
    }

    /// Whether `seat` is listed this tick (with or without an input).
    pub fn lists(&self, seat: Seat) -> bool {
        self.seats.binary_search_by_key(&seat, |e| e.seat).is_ok()
    }

    /// The same inputs with another input value in one seat (a test that flips one input, a tool
    /// that edits a log). `false` when the seat is not listed.
    pub fn replace(&mut self, seat: Seat, input: Option<I>) -> bool {
        match self.seats.binary_search_by_key(&seat, |e| e.seat) {
            Ok(i) => {
                self.seats[i].input = input;
                true
            }
            Err(_) => false,
        }
    }
}

/// A lockstep sim a log can drive: it saves itself, names its systems and steps from inputs.
///
/// Rules (the lockstep rules, ADR-8012 A-4): [`DetInputSim::step`] reads only the state and the
/// inputs (no clock, no IO, no thread, no hash-container iteration, transcendentals through
/// [`crate::math`]), and walks the seats in the order [`TickInputs::seats`] gives. Call
/// [`advance`] to step it, live as well as in a replay.
pub trait DetInputSim<const N: usize>: DetState + SystemFold<N> {
    /// One seat's input for one tick. Written into logs with the det-state codec
    /// ([`DetWriter::put`]): no `skip_serializing_if`, and only sequences of known length.
    type Input: Serialize + DeserializeOwned;

    /// Step tick `tick` (the state goes from boundary `tick` to `tick + 1`) with `inputs`
    /// (`inputs.tick() == tick`).
    fn step(&mut self, tick: u64, inputs: &TickInputs<Self::Input>);

    /// The world hash after a step (the wire checksum). Default: the systems folded into one word,
    /// [`SystemHashes::combined`]. A game with a checksum of its own returns it.
    fn hash(&self) -> u64 {
        <Self as DetInputSim<N>>::system_hashes(self).combined()
    }

    /// The per-system hashes after a step. Default: [`crate::hash::system_hashes`].
    fn system_hashes(&self) -> SystemHashes<N> {
        hash::system_hashes(self)
    }
}

/// The hashes after one tick stepped: the world hash and the per-system vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickHashes<const N: usize> {
    /// The tick that stepped.
    pub tick: u64,
    /// [`DetInputSim::hash`] after it.
    pub world: u64,
    /// [`DetInputSim::system_hashes`] after it.
    pub systems: SystemHashes<N>,
}

/// Step `sim` through tick `inputs.tick()` and hash it: the one step path. The live loop calls it
/// (or [`step_recorded`]), and so do [`replay`], [`CatchUp`] and the golden adapter.
pub fn advance<S: DetInputSim<N>, const N: usize>(
    sim: &mut S,
    inputs: &TickInputs<S::Input>,
) -> TickHashes<N> {
    let tick = inputs.tick();
    let _span = tracing::trace_span!("detsim_replay_advance", tick).entered();
    <S as DetInputSim<N>>::step(sim, tick, inputs);
    TickHashes {
        tick,
        world: <S as DetInputSim<N>>::hash(sim),
        systems: <S as DetInputSim<N>>::system_hashes(sim),
    }
}

/// The hashes a log recorded after one tick. Either part may be missing (a peer that kept only its
/// wire checksum, or only every `k`-th vector).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordedHash {
    /// The world hash, if recorded.
    pub world: Option<u64>,
    /// The per-system words in the sim's system order (exactly the log's system count), if recorded.
    pub systems: Option<Vec<u64>>,
}

impl RecordedHash {
    /// Both parts of `hashes`.
    pub fn of<const N: usize>(hashes: &TickHashes<N>) -> Self {
        RecordedHash {
            world: Some(hashes.world),
            systems: Some(hashes.systems.words.to_vec()),
        }
    }

    /// Nothing recorded.
    pub fn is_empty(&self) -> bool {
        self.world.is_none() && self.systems.is_none()
    }
}

/// One tick of a log: its inputs and what was recorded after it stepped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry<I> {
    /// The inputs (their tick is the entry's tick).
    pub inputs: TickInputs<I>,
    /// The hashes recorded after the step.
    pub hash: RecordedHash,
}

/// Ticks with their inputs and recorded hashes, consecutive from `start_tick`, for one sim type.
///
/// The log names the sim it drives (its `STATE_TAG` and `STATE_VERSION`) and its system count, and
/// may carry the framed snapshot it starts from ([`InputLog::start_snapshot`]). Encoded as a det-state
/// frame of kind `InputLog`: the header's integrity hash refuses damaged bytes before the body is read.
///
/// Body layout (after the 40-byte frame header, whose tick is `start_tick`): sim tag (8 bytes), sim
/// version `u32`, system count `u32`, the start snapshot as a length-prefixed byte string (empty for
/// none), the entry count, then per entry: the seat count, per seat `u16` seat, a `0`/`1` presence
/// byte and the input in the det-state codec; a flags byte (bit 0 world hash, bit 1 system words, other
/// bits zero), the world hash `u64` if flagged, the system words if flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLog<I> {
    sim_tag: [u8; 8],
    sim_version: u32,
    system_count: u32,
    start_tick: u64,
    start_snapshot: Vec<u8>,
    entries: Vec<LogEntry<I>>,
}

/// An input log's frame and identity, read without decoding the entries (no input type needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLogHeader {
    /// The sim's `STATE_TAG`.
    pub sim_tag: [u8; 8],
    /// The sim's `STATE_VERSION`.
    pub sim_version: u32,
    /// The sim's system count.
    pub system_count: u32,
    /// The first tick the log steps (the boundary its snapshot was taken at).
    pub start_tick: u64,
    /// Whether the log carries its start snapshot.
    pub has_snapshot: bool,
}

const FLAG_WORLD: u8 = 1;
const FLAG_SYSTEMS: u8 = 2;

impl<I> InputLog<I> {
    /// An empty log for sim type `S` (its tag, version and system count), starting at boundary
    /// `start_tick`.
    pub fn for_sim<S, const N: usize>(start_tick: u64) -> Self
    where
        S: DetInputSim<N, Input = I>,
    {
        InputLog {
            sim_tag: S::STATE_TAG,
            sim_version: S::STATE_VERSION,
            system_count: N as u32,
            start_tick,
            start_snapshot: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// Carry `snapshot` (a frame from [`state::snapshot`]) as the state the log starts from. Its
    /// header must check out, be a `State` frame of this log's sim and be taken at `start_tick`
    /// (`bad_tag`, `bad_version`, `tick_gap`, or the header's own codes).
    pub fn set_start_snapshot(&mut self, snapshot: Vec<u8>) -> Result<(), ReplayError> {
        check_snapshot_header(
            &snapshot,
            DetReadLimits::DEFAULT,
            self.sim_tag,
            self.sim_version,
            self.start_tick,
        )?;
        self.start_snapshot = snapshot;
        Ok(())
    }

    /// The carried start snapshot, if any.
    pub fn start_snapshot(&self) -> Option<&[u8]> {
        (!self.start_snapshot.is_empty()).then_some(self.start_snapshot.as_slice())
    }

    /// The sim's `STATE_TAG`.
    pub const fn sim_tag(&self) -> [u8; 8] {
        self.sim_tag
    }

    /// The sim's `STATE_VERSION`.
    pub const fn sim_version(&self) -> u32 {
        self.sim_version
    }

    /// The sim's system count.
    pub const fn system_count(&self) -> u32 {
        self.system_count
    }

    /// The first tick the log steps.
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    /// The boundary after the last entry (`start_tick + len`).
    pub fn end_tick(&self) -> u64 {
        self.start_tick + self.entries.len() as u64
    }

    /// Ticks held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no tick is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, oldest first.
    pub fn entries(&self) -> &[LogEntry<I>] {
        &self.entries
    }

    /// The entry for `tick`, if held.
    pub fn entry(&self, tick: u64) -> Option<&LogEntry<I>> {
        let i = usize::try_from(tick.checked_sub(self.start_tick)?).ok()?;
        self.entries.get(i)
    }

    /// Mutable access to the entry for `tick` (a tool that edits a log, a test that flips an input).
    pub fn entry_mut(&mut self, tick: u64) -> Option<&mut LogEntry<I>> {
        let i = usize::try_from(tick.checked_sub(self.start_tick)?).ok()?;
        self.entries.get_mut(i)
    }

    /// Append the next tick. Its tick must be [`InputLog::end_tick`] (`tick_gap`), and recorded
    /// system words must number the log's system count (`system_count`).
    pub fn push(&mut self, entry: LogEntry<I>) -> Result<(), ReplayError> {
        let expected = self.end_tick();
        if entry.inputs.tick() != expected {
            return Err(ReplayError::TickGap {
                expected,
                found: entry.inputs.tick(),
            });
        }
        if let Some(words) = &entry.hash.systems
            && words.len() != self.system_count as usize
        {
            return Err(ReplayError::SystemCount {
                found: words.len() as u32,
                expected: self.system_count,
            });
        }
        self.entries.push(entry);
        Ok(())
    }

    /// The log from boundary `tick` on (entries `tick..end`), with no start snapshot: a desync bundle
    /// is the newest checkpoint at or before the split ([`crate::state::SnapshotRing`]) plus this.
    /// `tick` outside `start_tick..=end_tick` is `bad_until`.
    pub fn suffix(&self, tick: u64) -> Result<InputLog<I>, ReplayError>
    where
        I: Clone,
    {
        let (start, end) = (self.start_tick, self.end_tick());
        if tick < start || tick > end {
            return Err(ReplayError::BadUntil {
                until: tick,
                start,
                end,
            });
        }
        let skip = usize::try_from(tick - start).unwrap_or(usize::MAX);
        Ok(InputLog {
            sim_tag: self.sim_tag,
            sim_version: self.sim_version,
            system_count: self.system_count,
            start_tick: tick,
            start_snapshot: Vec::new(),
            entries: self.entries.iter().skip(skip).cloned().collect(),
        })
    }

    /// Drop every entry from `tick` on (a rewind before re-recording). Ticks before the start drop
    /// everything.
    pub fn truncate(&mut self, tick: u64) {
        let keep = tick.saturating_sub(self.start_tick);
        self.entries.truncate(usize::try_from(keep).unwrap_or(usize::MAX));
    }
}

impl<I: Serialize> InputLog<I> {
    /// Encode the log as a framed det-state body (see the type's docs for the layout). Fails only
    /// when a length does not fit a `u32` or an input refuses the codec (`bad_value`).
    pub fn encode(&self) -> Result<Vec<u8>, DetStateError> {
        let _span = tracing::debug_span!("detsim_input_log_encode", ticks = self.entries.len()).entered();
        let mut w = DetWriter::new();
        w.raw(&self.sim_tag);
        w.u32(self.sim_version);
        w.u32(self.system_count);
        w.bytes(&self.start_snapshot);
        w.length(self.entries.len());
        for entry in &self.entries {
            w.length(entry.inputs.seats.len());
            for seat in &entry.inputs.seats {
                w.u16(seat.seat);
                match &seat.input {
                    Some(input) => {
                        w.u8(1);
                        w.put(input);
                    }
                    None => w.u8(0),
                }
            }
            let mut flags = 0u8;
            if entry.hash.world.is_some() {
                flags |= FLAG_WORLD;
            }
            if entry.hash.systems.is_some() {
                flags |= FLAG_SYSTEMS;
            }
            w.u8(flags);
            if let Some(world) = entry.hash.world {
                w.u64(world);
            }
            if let Some(words) = &entry.hash.systems {
                if words.len() != self.system_count as usize {
                    w.fail(DetStateError::bad_value(format_args!(
                        "{} system words at tick {}, the log has {} systems",
                        words.len(),
                        entry.inputs.tick(),
                        self.system_count
                    )));
                }
                for &word in words {
                    w.u64(word);
                }
            }
        }
        state::frame(
            SnapshotKind::InputLog,
            INPUT_LOG_TAG,
            INPUT_LOG_VERSION,
            self.start_tick,
            w,
        )
    }
}

/// Read the fixed part of an input log body after its frame.
fn read_identity(r: &mut DetReader<'_>, start_tick: u64) -> Result<InputLogHeader, DetStateError> {
    let sim_tag = r.array::<8>()?;
    let sim_version = r.u32()?;
    let system_count = r.u32()?;
    if system_count as usize > hash::MAX_SYSTEMS {
        return Err(DetStateError::bad_value(format_args!(
            "{system_count} systems, at most {}",
            hash::MAX_SYSTEMS
        )));
    }
    Ok(InputLogHeader {
        sim_tag,
        sim_version,
        system_count,
        start_tick,
        has_snapshot: false,
    })
}

/// Read an input log's identity (sim tag, version, system count, start tick, whether it carries a
/// snapshot) after checking its frame. Does not decode the entries.
pub fn read_input_log_header(bytes: &[u8], limits: DetReadLimits) -> Result<InputLogHeader, DetStateError> {
    let (start_tick, mut r) = state::open_frame(
        bytes,
        limits,
        SnapshotKind::InputLog,
        INPUT_LOG_TAG,
        INPUT_LOG_VERSION,
    )?;
    let mut header = read_identity(&mut r, start_tick)?;
    header.has_snapshot = !r.bytes()?.is_empty();
    Ok(header)
}

impl<I: DeserializeOwned> InputLog<I> {
    /// [`InputLog::decode_with`] under [`DetReadLimits::DEFAULT`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        Self::decode_with(bytes, DetReadLimits::DEFAULT)
    }

    /// Decode a log written by [`InputLog::encode`]. Bounded: every length is checked against
    /// `limits` and the bytes left before anything is sized by it. Refuses a damaged or foreign frame
    /// (the det-state codes), more than [`crate::hash::MAX_SYSTEMS`] systems or a bad flags byte
    /// (`bad_value`), more than [`MAX_SEATS_PER_TICK`] seats (`length_limit`), seats out of order
    /// (`seat_order`), a carried snapshot of another sim or tick, and unread bytes
    /// (`trailing_bytes`). Never panics on input bytes.
    pub fn decode_with(bytes: &[u8], limits: DetReadLimits) -> Result<Self, ReplayError> {
        let _span = tracing::debug_span!("detsim_input_log_decode", len = bytes.len()).entered();
        let (start_tick, mut r) = state::open_frame(
            bytes,
            limits,
            SnapshotKind::InputLog,
            INPUT_LOG_TAG,
            INPUT_LOG_VERSION,
        )?;
        let id = read_identity(&mut r, start_tick)?;
        let start_snapshot = r.bytes()?.to_vec();
        if !start_snapshot.is_empty() {
            check_snapshot_header(&start_snapshot, limits, id.sim_tag, id.sim_version, start_tick)?;
        }
        let system_count = id.system_count as usize;
        let count = r.length()?;
        // An entry is at least 5 bytes (seat count and flags): a forged count cannot size the vector.
        let mut entries = Vec::with_capacity(
            count
                .min(r.remaining() / 5)
                .min(state::PREALLOC_BYTES / std::mem::size_of::<LogEntry<I>>().max(1)),
        );
        for i in 0..count {
            let tick = start_tick
                .checked_add(i as u64)
                .ok_or_else(|| DetStateError::bad_value("log ticks overflow u64"))?;
            let seats = r.length()?;
            if seats > MAX_SEATS_PER_TICK {
                return Err(DetStateError::LengthLimit {
                    len: seats as u64,
                    limit: MAX_SEATS_PER_TICK as u64,
                }
                .into());
            }
            let mut inputs = TickInputs {
                tick,
                seats: Vec::with_capacity(seats.min(r.remaining() / 3).min(64)),
            };
            for _ in 0..seats {
                let seat = r.u16()?;
                let input = match r.u8()? {
                    0 => None,
                    1 => Some(r.take::<I>()?),
                    other => {
                        return Err(DetStateError::bad_value(format_args!(
                            "input presence byte {other} at tick {tick}"
                        ))
                        .into());
                    }
                };
                inputs.push(seat, input)?;
            }
            let flags = r.u8()?;
            if flags & !(FLAG_WORLD | FLAG_SYSTEMS) != 0 {
                return Err(
                    DetStateError::bad_value(format_args!("hash flags {flags:#04x} at tick {tick}")).into(),
                );
            }
            let world = if flags & FLAG_WORLD != 0 {
                Some(r.u64()?)
            } else {
                None
            };
            let systems = if flags & FLAG_SYSTEMS != 0 {
                let need = system_count * 8;
                if need > r.remaining() {
                    return Err(DetStateError::Truncated {
                        offset: r.position(),
                        needed: need,
                    }
                    .into());
                }
                let mut words = Vec::with_capacity(system_count);
                for _ in 0..system_count {
                    words.push(r.u64()?);
                }
                Some(words)
            } else {
                None
            };
            entries.push(LogEntry {
                inputs,
                hash: RecordedHash { world, systems },
            });
        }
        r.finish()?;
        Ok(InputLog {
            sim_tag: id.sim_tag,
            sim_version: id.sim_version,
            system_count: id.system_count,
            start_tick,
            start_snapshot,
            entries,
        })
    }
}

/// Check a carried or given start snapshot: a sound `State` frame of `tag` / `version` at `tick`.
fn check_snapshot_header(
    snapshot: &[u8],
    limits: DetReadLimits,
    tag: [u8; 8],
    version: u32,
    tick: u64,
) -> Result<(), ReplayError> {
    let header = state::read_header(snapshot, limits)?;
    if header.kind != SnapshotKind::State {
        return Err(DetStateError::bad_value(format_args!(
            "the start snapshot is a {:?} frame, expected a State frame",
            header.kind
        ))
        .into());
    }
    if header.tag != tag {
        return Err(DetStateError::BadTag {
            found: header.tag,
            expected: tag,
        }
        .into());
    }
    if header.version != version {
        return Err(DetStateError::BadVersion {
            found: header.version,
            expected: version,
        }
        .into());
    }
    if header.tick != tick {
        return Err(ReplayError::TickGap {
            expected: tick,
            found: header.tick,
        });
    }
    Ok(())
}

/// Why a log could not be recorded, read or replayed. [`ReplayError::code`] is the stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The bytes (log or snapshot) were refused by the det-state codec: its own code (`truncated`,
    /// `length_limit`, `bad_version`, `bad_tag`, `trailing_bytes`, `bad_value`, `bad_hash`).
    State(DetStateError),
    /// The log was recorded for another sim type or version. Code `wrong_sim`.
    WrongSim {
        /// The tag and version in the log.
        found: ([u8; 8], u32),
        /// The tag and version of the sim replaying it.
        expected: ([u8; 8], u32),
    },
    /// The log's system count is not the sim's. Code `system_count`.
    SystemCount {
        /// The count in the log (or the words in an entry).
        found: u32,
        /// The count expected.
        expected: u32,
    },
    /// A tick is not the next one: a log entry, a catch-up frame, or a snapshot that does not start
    /// where the log does. Code `tick_gap`.
    TickGap {
        /// The tick expected.
        expected: u64,
        /// The tick found.
        found: u64,
    },
    /// A seat is listed twice or out of ascending order. Code `seat_order`.
    SeatOrder {
        /// The tick.
        tick: u64,
        /// The seat that broke the order.
        seat: Seat,
    },
    /// Neither the log nor the caller gave a start snapshot. Code `no_snapshot`.
    NoSnapshot,
    /// `until` is outside the log (`start_tick..=end_tick`). Code `bad_until`.
    BadUntil {
        /// The boundary asked for.
        until: u64,
        /// The log's first tick.
        start: u64,
        /// The log's end boundary.
        end: u64,
    },
    /// No hook is registered for the sim tag. Code `unknown_sim`.
    UnknownSim {
        /// The tag looked up.
        tag: [u8; 8],
    },
    /// A hook is already registered for the sim tag. Code `duplicate_sim`.
    DuplicateSim {
        /// The tag.
        tag: [u8; 8],
    },
}

impl ReplayError {
    /// Stable lowercase ASCII code (the det-state codes pass through).
    pub const fn code(&self) -> &'static str {
        match self {
            ReplayError::State(e) => e.code(),
            ReplayError::WrongSim { .. } => "wrong_sim",
            ReplayError::SystemCount { .. } => "system_count",
            ReplayError::TickGap { .. } => "tick_gap",
            ReplayError::SeatOrder { .. } => "seat_order",
            ReplayError::NoSnapshot => "no_snapshot",
            ReplayError::BadUntil { .. } => "bad_until",
            ReplayError::UnknownSim { .. } => "unknown_sim",
            ReplayError::DuplicateSim { .. } => "duplicate_sim",
        }
    }
}

/// Printable form of a tag: ASCII graphics kept, NULs dropped, other bytes as `\xNN`.
fn tag_text(tag: &[u8; 8]) -> String {
    let mut out = String::new();
    for &b in tag {
        if b.is_ascii_graphic() {
            out.push(char::from(b));
        } else if b != 0 {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::State(e) => write!(f, "{e}"),
            ReplayError::WrongSim { found, expected } => write!(
                f,
                "wrong_sim: the log drives \"{}\" version {}, this sim is \"{}\" version {}",
                tag_text(&found.0),
                found.1,
                tag_text(&expected.0),
                expected.1
            ),
            ReplayError::SystemCount { found, expected } => {
                write!(f, "system_count: {found} system words, expected {expected}")
            }
            ReplayError::TickGap { expected, found } => {
                write!(f, "tick_gap: expected tick {expected}, found tick {found}")
            }
            ReplayError::SeatOrder { tick, seat } => {
                write!(f, "seat_order: seat {seat} out of ascending order at tick {tick}")
            }
            ReplayError::NoSnapshot => {
                f.write_str("no_snapshot: the log carries no start snapshot and none was given")
            }
            ReplayError::BadUntil { until, start, end } => {
                write!(f, "bad_until: {until} is outside the log's ticks {start}..={end}")
            }
            ReplayError::UnknownSim { tag } => {
                write!(f, "unknown_sim: no replay hook for \"{}\"", tag_text(tag))
            }
            ReplayError::DuplicateSim { tag } => {
                write!(
                    f,
                    "duplicate_sim: a replay hook for \"{}\" is already registered",
                    tag_text(tag)
                )
            }
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<DetStateError> for ReplayError {
    fn from(e: DetStateError) -> Self {
        ReplayError::State(e)
    }
}

/// Step `sim` through `inputs` with [`advance`] and append the tick to `log` (with its hashes when
/// `keep_hashes`). The live loop's recording step; the tick must be the log's next (`tick_gap`).
/// The sim has stepped even when the push fails.
pub fn step_recorded<S: DetInputSim<N>, const N: usize>(
    sim: &mut S,
    inputs: TickInputs<S::Input>,
    log: &mut InputLog<S::Input>,
    keep_hashes: bool,
) -> Result<TickHashes<N>, ReplayError> {
    let expected = log.end_tick();
    if inputs.tick() != expected {
        return Err(ReplayError::TickGap {
            expected,
            found: inputs.tick(),
        });
    }
    let hashes = advance(sim, &inputs);
    let hash = if keep_hashes {
        RecordedHash::of(&hashes)
    } else {
        RecordedHash::default()
    };
    log.push(LogEntry { inputs, hash })?;
    Ok(hashes)
}

/// What a replay found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// The boundary the replay started from (the log's start tick).
    pub start_tick: u64,
    /// Ticks stepped (up to and including a diverging tick).
    pub ticks_run: u64,
    /// The boundary the sim stopped at (`start_tick + ticks_run`).
    pub end_tick: u64,
    /// Ticks whose recomputed hashes were compared with recorded ones.
    pub hashes_compared: u64,
    /// The first tick whose recomputed hash differs from the recorded one, if any. The replay stops
    /// after it.
    pub first_diverging_tick: Option<u64>,
    /// The first system that differs at that tick (ties in the sim's system order); `None` when the
    /// log recorded no system words there or only the world hash differs.
    pub system: Option<&'static str>,
    /// Every system that differs at that tick, in the sim's system order.
    pub systems: Vec<Divergence>,
    /// The last compared tick before the divergence on which everything agreed.
    pub last_agreed: Option<u64>,
    /// The world hash after the last tick stepped (`None` when no tick ran).
    pub final_world: Option<u64>,
}

impl ReplayReport {
    /// No recorded hash differed.
    pub const fn passed(&self) -> bool {
        self.first_diverging_tick.is_none()
    }

    /// One `key value` line for logs and agents:
    /// `replay verdict ok start 0 end 600 ticks 600 compared 600 first none system none last_agreed 599`.
    pub fn verdict_line(&self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "none".to_string(), |t| t.to_string());
        format!(
            "replay verdict {} start {} end {} ticks {} compared {} first {} system {} last_agreed {}",
            if self.passed() { "ok" } else { "diverged" },
            self.start_tick,
            self.end_tick,
            self.ticks_run,
            self.hashes_compared,
            opt(self.first_diverging_tick),
            self.system.unwrap_or("none"),
            opt(self.last_agreed),
        )
    }
}

/// Check that `log` was recorded for `S` with `N` systems.
fn check_log_identity<S: DetInputSim<N>, const N: usize>(
    sim_tag: [u8; 8],
    sim_version: u32,
    system_count: u32,
) -> Result<(), ReplayError> {
    if (sim_tag, sim_version) != (S::STATE_TAG, S::STATE_VERSION) {
        return Err(ReplayError::WrongSim {
            found: (sim_tag, sim_version),
            expected: (S::STATE_TAG, S::STATE_VERSION),
        });
    }
    if system_count as usize != N {
        return Err(ReplayError::SystemCount {
            found: system_count,
            expected: N as u32,
        });
    }
    Ok(())
}

/// Replay `log` on `sim`, which must be at the log's start boundary (restored from its snapshot),
/// up to boundary `until` (default: the log's end). Every tick steps through [`advance`]; where the
/// log recorded hashes, the recomputed ones are compared, and the replay stops after the first tick
/// that differs. The report names that tick, the system ([`crate::hash::diverged_systems`] on the
/// recorded and recomputed rows) and the last tick that agreed.
///
/// Refuses a log of another sim (`wrong_sim`, `system_count`) and an `until` outside the log
/// (`bad_until`); the sim is untouched then.
pub fn replay<S: DetInputSim<N>, const N: usize>(
    sim: &mut S,
    log: &InputLog<S::Input>,
    until: Option<u64>,
) -> Result<ReplayReport, ReplayError> {
    check_log_identity::<S, N>(log.sim_tag, log.sim_version, log.system_count)?;
    let start = log.start_tick();
    let end = log.end_tick();
    let until = until.unwrap_or(end);
    if until < start || until > end {
        return Err(ReplayError::BadUntil { until, start, end });
    }
    let _span = tracing::debug_span!("detsim_replay", start, until).entered();
    let mut report = ReplayReport {
        start_tick: start,
        ticks_run: 0,
        end_tick: start,
        hashes_compared: 0,
        first_diverging_tick: None,
        system: None,
        systems: Vec::new(),
        last_agreed: None,
        final_world: None,
    };
    let steps = usize::try_from(until - start).unwrap_or(usize::MAX);
    for entry in log.entries.iter().take(steps) {
        let now = advance(sim, &entry.inputs);
        report.ticks_run += 1;
        report.end_tick = now.tick + 1;
        report.final_world = Some(now.world);
        if entry.hash.is_empty() {
            continue;
        }
        report.hashes_compared += 1;
        let mut split = entry.hash.world.is_some_and(|w| w != now.world);
        if let Some(words) = &entry.hash.systems {
            let mut recorded = SystemHashes::<N>::default();
            // `push` and `decode_with` hold the count to the log's, which is `N` here.
            for (slot, word) in recorded.words.iter_mut().zip(words) {
                *slot = *word;
            }
            if recorded != now.systems {
                split = true;
                let found = hash::diverged_systems(
                    &S::SYSTEMS,
                    &[TraceRow {
                        tick: now.tick,
                        hashes: recorded,
                    }],
                    &[TraceRow {
                        tick: now.tick,
                        hashes: now.systems,
                    }],
                );
                report.system = found.root().map(|d| d.system);
                report.systems = found.systems;
            }
        }
        if split {
            report.first_diverging_tick = Some(now.tick);
            break;
        }
        report.last_agreed = Some(now.tick);
    }
    Ok(report)
}

/// Restore `S` from `snapshot` (or the log's carried snapshot when `None`) and [`replay`] `log`.
/// The snapshot must be a `State` frame of `S` taken at the log's start tick (`tick_gap` otherwise).
pub fn replay_from_snapshot<S: DetInputSim<N>, const N: usize>(
    snapshot: Option<&[u8]>,
    log: &InputLog<S::Input>,
    until: Option<u64>,
    limits: DetReadLimits,
) -> Result<ReplayReport, ReplayError> {
    check_log_identity::<S, N>(log.sim_tag, log.sim_version, log.system_count)?;
    let bytes = snapshot.or(log.start_snapshot()).ok_or(ReplayError::NoSnapshot)?;
    let (tick, mut sim) = state::restore_with::<S>(bytes, limits)?;
    if tick != log.start_tick() {
        return Err(ReplayError::TickGap {
            expected: log.start_tick(),
            found: tick,
        });
    }
    crate::fpu::ensure_default_fp_env();
    replay(&mut sim, log, until)
}

/// Decode `log_bytes` and [`replay_from_snapshot`]: the whole offline repro from bytes.
pub fn replay_bytes<S: DetInputSim<N>, const N: usize>(
    log_bytes: &[u8],
    snapshot: Option<&[u8]>,
    until: Option<u64>,
    limits: DetReadLimits,
) -> Result<ReplayReport, ReplayError> {
    let header = read_input_log_header(log_bytes, limits)?;
    check_log_identity::<S, N>(header.sim_tag, header.sim_version, header.system_count)?;
    let log = InputLog::<S::Input>::decode_with(log_bytes, limits)?;
    replay_from_snapshot::<S, N>(snapshot, &log, until, limits)
}

/// Frames waiting for a sim that is behind the live tick (a late joiner, a resynced seat), stepped
/// at most a budget of ticks per call (spec 120 § 7.4).
///
/// Frames are queued in tick order as they arrive ([`CatchUp::push`] refuses a gap); each
/// [`CatchUp::run`] steps at most `budget` of them through [`advance`]. The budget is a tick count,
/// not a time, so how far a call gets never depends on the machine; the host picks the budget and how
/// many calls a frame makes.
#[derive(Debug, Clone)]
pub struct CatchUp<I> {
    next_tick: u64,
    frames: VecDeque<TickInputs<I>>,
    calls: u64,
    stepped: u64,
}

/// What one [`CatchUp::run`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpStep {
    /// Ticks stepped by this call (at most the budget).
    pub stepped: u32,
    /// The boundary the sim is at after the call.
    pub tick: u64,
    /// Frames still queued.
    pub pending: usize,
    /// The world hash after the last tick this call stepped.
    pub world: Option<u64>,
}

impl CatchUpStep {
    /// Every queued frame has been stepped.
    pub const fn caught_up(&self) -> bool {
        self.pending == 0
    }
}

impl<I> CatchUp<I> {
    /// A queue for a sim at boundary `tick` (the tick of the snapshot it was restored from).
    pub const fn new(tick: u64) -> Self {
        CatchUp {
            next_tick: tick,
            frames: VecDeque::new(),
            calls: 0,
            stepped: 0,
        }
    }

    /// Queue the next frame. Its tick must follow the last queued one (or the sim's boundary when
    /// none is queued): `tick_gap` otherwise, and nothing is queued.
    pub fn push(&mut self, frame: TickInputs<I>) -> Result<(), ReplayError> {
        let expected = self.next_tick + self.frames.len() as u64;
        if frame.tick() != expected {
            return Err(ReplayError::TickGap {
                expected,
                found: frame.tick(),
            });
        }
        self.frames.push_back(frame);
        Ok(())
    }

    /// The boundary the sim is at.
    pub const fn tick(&self) -> u64 {
        self.next_tick
    }

    /// Frames queued and not yet stepped.
    pub fn pending(&self) -> usize {
        self.frames.len()
    }

    /// Calls to [`CatchUp::run`] so far.
    pub const fn calls(&self) -> u64 {
        self.calls
    }

    /// Ticks stepped so far.
    pub const fn stepped(&self) -> u64 {
        self.stepped
    }

    /// Step at most `budget` queued frames on `sim` (which must be at [`CatchUp::tick`]).
    pub fn run<S, const N: usize>(&mut self, sim: &mut S, budget: u32) -> CatchUpStep
    where
        S: DetInputSim<N, Input = I>,
    {
        let _span = tracing::trace_span!("detsim_catch_up", tick = self.next_tick, budget).entered();
        self.calls += 1;
        let mut stepped = 0u32;
        let mut world = None;
        while stepped < budget {
            let Some(frame) = self.frames.pop_front() else {
                break;
            };
            let now = advance(sim, &frame);
            world = Some(now.world);
            self.next_tick = now.tick + 1;
            stepped += 1;
        }
        self.stepped += u64::from(stepped);
        CatchUpStep {
            stepped,
            tick: self.next_tick,
            pending: self.frames.len(),
            world,
        }
    }
}

/// Step `sim` through at most `budget_per_call` of the frames queued in `frames` ([`CatchUp::run`]).
pub fn catch_up<S: DetInputSim<N>, const N: usize>(
    sim: &mut S,
    frames: &mut CatchUp<S::Input>,
    budget_per_call: u32,
) -> CatchUpStep {
    frames.run(sim, budget_per_call)
}

/// The hook a desync tool registers per sim type: re-run the game step headless from a snapshot plus
/// an input log and report where it parts from the recorded hashes.
///
/// Object-safe and bytes-in, so a tool that holds a desync bundle (a snapshot and an input log from
/// the peers) finds the hook by the sim tag in the log ([`read_input_log_header`]) without knowing the
/// game's types. [`ReplayHook`] implements it for any [`DetInputSim`]; a game registers
/// `ReplayHook::<MySim, N>::new()` in a [`DesyncReplayRegistry`].
pub trait DesyncReplay: Send + Sync {
    /// The sim's `STATE_TAG`.
    fn sim_tag(&self) -> [u8; 8];
    /// The sim's `STATE_VERSION`.
    fn sim_version(&self) -> u32;
    /// The sim's system names, in vector order.
    fn system_names(&self) -> Vec<&'static str>;
    /// Decode `log`, restore the sim from `snapshot` (or the log's own when `None`) and replay up to
    /// boundary `until` (default: the log's end) under `limits`.
    fn replay(
        &self,
        snapshot: Option<&[u8]>,
        log: &[u8],
        until: Option<u64>,
        limits: DetReadLimits,
    ) -> Result<ReplayReport, ReplayError>;
}

/// [`DesyncReplay`] for a [`DetInputSim`] type `S` with `N` systems.
pub struct ReplayHook<S, const N: usize> {
    _sim: PhantomData<fn() -> S>,
}

impl<S, const N: usize> ReplayHook<S, N> {
    /// The hook.
    pub const fn new() -> Self {
        ReplayHook { _sim: PhantomData }
    }
}

impl<S, const N: usize> Default for ReplayHook<S, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, const N: usize> fmt::Debug for ReplayHook<S, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplayHook").field("systems", &N).finish()
    }
}

impl<S: DetInputSim<N>, const N: usize> DesyncReplay for ReplayHook<S, N> {
    fn sim_tag(&self) -> [u8; 8] {
        S::STATE_TAG
    }
    fn sim_version(&self) -> u32 {
        S::STATE_VERSION
    }
    fn system_names(&self) -> Vec<&'static str> {
        S::SYSTEMS.names().to_vec()
    }
    fn replay(
        &self,
        snapshot: Option<&[u8]>,
        log: &[u8],
        until: Option<u64>,
        limits: DetReadLimits,
    ) -> Result<ReplayReport, ReplayError> {
        replay_bytes::<S, N>(log, snapshot, until, limits)
    }
}

/// Replay hooks by sim tag, kept sorted by tag (no hash map).
#[derive(Default)]
pub struct DesyncReplayRegistry {
    hooks: Vec<Box<dyn DesyncReplay>>,
}

impl fmt::Debug for DesyncReplayRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tags: Vec<String> = self.hooks.iter().map(|h| tag_text(&h.sim_tag())).collect();
        f.debug_struct("DesyncReplayRegistry")
            .field("sims", &tags)
            .finish()
    }
}

impl DesyncReplayRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `hook`; a second hook for the same sim tag is `duplicate_sim`.
    pub fn register(&mut self, hook: Box<dyn DesyncReplay>) -> Result<(), ReplayError> {
        let tag = hook.sim_tag();
        match self.hooks.binary_search_by_key(&tag, |h| h.sim_tag()) {
            Ok(_) => Err(ReplayError::DuplicateSim { tag }),
            Err(i) => {
                self.hooks.insert(i, hook);
                Ok(())
            }
        }
    }

    /// The hook for `tag`.
    pub fn get(&self, tag: [u8; 8]) -> Option<&dyn DesyncReplay> {
        let i = self.hooks.binary_search_by_key(&tag, |h| h.sim_tag()).ok()?;
        Some(self.hooks[i].as_ref())
    }

    /// Registered sim tags, ascending.
    pub fn tags(&self) -> Vec<[u8; 8]> {
        self.hooks.iter().map(|h| h.sim_tag()).collect()
    }

    /// Read the log's sim tag, find its hook (`unknown_sim`) and replay.
    pub fn replay(
        &self,
        snapshot: Option<&[u8]>,
        log: &[u8],
        until: Option<u64>,
        limits: DetReadLimits,
    ) -> Result<ReplayReport, ReplayError> {
        let header = read_input_log_header(log, limits)?;
        let hook = self
            .get(header.sim_tag)
            .ok_or(ReplayError::UnknownSim { tag: header.sim_tag })?;
        hook.replay(snapshot, log, until, limits)
    }
}
