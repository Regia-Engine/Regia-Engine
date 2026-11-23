// SPDX-License-Identifier: MPL-2.0
//! The authoritative tick driver and its input buffers.
//!
//! The server owns the tick counter. Clients stamp each input with the tick it is meant for and
//! send it ahead of time; every input packet repeats the last few inputs ([`InputBundle`]), so one
//! lost packet heals from the next (the redundant-input technique of the Overwatch GDC 2017
//! netcode talk and G. Fiedler's "Deterministic Lockstep", gafferongames.com, 2014). Each tick the
//! driver hands the simulation the inputs that arrived for that tick, in peer-id order; a peer
//! whose input is missing is counted and left out, and the game decides how to fill the gap.
//!
//! Bundles are size-aware (ADR-7896): [`InputHistory::bundles_within`] cuts the history into
//! runs that each fit one datagram, newest first, and an input larger than a datagram travels on
//! the reliable input lane (`tick u64 | input`, at most [`MAX_RELIABLE_INPUT_BYTES`]).
//!
//! With a late merge set ([`TickDriver::set_late_merge`], spec 120 § 6.3), an input that arrives
//! after its tick was simulated without it is not dropped: it is merged into the next tick the
//! driver builds, so a button press is never lost (a duplicate of it merges nothing).
//!
//! Time enters through [`TickClock`] as elapsed milliseconds, so tests step it exactly.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::payload::NetCodec;
use crate::session::PeerId;
use crate::wire::{WireError, WireReader, WireWriter};

/// Inputs gathered for one tick, by peer id. Peers whose input never arrived are absent.
pub type TickInputs<I> = BTreeMap<PeerId, I>;

/// Most inputs one bundle may carry.
pub const MAX_BUNDLE_INPUTS: usize = 32;

/// Tick history kept for [`TickDriver::inputs_at`].
pub const TICK_HISTORY: usize = 64;

/// How far back a late input may be merged (spec 120 § 6.3): about 4 s at 60 Hz, a one-second
/// stall on a 300 ms path with room to spare. Older late inputs are dropped.
pub const LATE_MERGE_TICKS: u64 = 256;

/// One peer's pending inputs, sorted by tick.
#[derive(Debug, Clone)]
pub struct InputBuffer<I> {
    pending: VecDeque<(u64, I)>,
    capacity: usize,
    consumed_through: Option<u64>,
}

impl<I> InputBuffer<I> {
    /// A buffer holding at most `capacity` future ticks.
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
            consumed_through: None,
        }
    }

    /// Buffers `input` for `tick`. Duplicates, ticks already simulated and ticks older than a full
    /// buffer are ignored; returns whether it was stored.
    pub fn insert(&mut self, tick: u64, input: I) -> bool {
        if self.consumed_through.is_some_and(|c| tick <= c) {
            return false;
        }
        match self.pending.binary_search_by_key(&tick, |(t, _)| *t) {
            Ok(_) => false,
            Err(mut pos) => {
                if self.pending.len() >= self.capacity {
                    if pos == 0 {
                        return false;
                    }
                    self.pending.pop_front();
                    pos -= 1;
                }
                self.pending.insert(pos, (tick, input));
                true
            }
        }
    }

    /// Takes the input for `tick` and discards anything older; `None` if it never arrived.
    pub fn take(&mut self, tick: u64) -> Option<I> {
        self.consumed_through = Some(self.consumed_through.map_or(tick, |c| c.max(tick)));
        while self.pending.front().is_some_and(|(t, _)| *t < tick) {
            self.pending.pop_front();
        }
        if self.pending.front().is_some_and(|(t, _)| *t == tick) {
            return self.pending.pop_front().map(|(_, i)| i);
        }
        None
    }

    /// True when an input for `tick` is waiting.
    pub fn has(&self, tick: u64) -> bool {
        self.pending.binary_search_by_key(&tick, |(t, _)| *t).is_ok()
    }

    /// Oldest buffered tick.
    pub fn first_tick(&self) -> Option<u64> {
        self.pending.front().map(|(t, _)| *t)
    }

    /// Inputs waiting.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True when nothing waits.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Fixed-step accumulator: turns elapsed milliseconds into whole ticks.
#[derive(Debug, Clone)]
pub struct TickClock {
    tick_rate_hz: u32,
    accumulated_us: u64,
    max_catch_up: u32,
}

impl TickClock {
    /// A clock at `tick_rate_hz` (clamped to 1..=1000) that runs at most `max_catch_up` ticks per
    /// call (the rest of a long stall is dropped rather than spiraling).
    pub fn new(tick_rate_hz: u32, max_catch_up: u32) -> Self {
        Self {
            tick_rate_hz: tick_rate_hz.clamp(1, 1000),
            accumulated_us: 0,
            max_catch_up: max_catch_up.max(1),
        }
    }

    /// Tick rate.
    pub fn tick_rate_hz(&self) -> u32 {
        self.tick_rate_hz
    }

    /// Length of one tick in microseconds.
    pub fn tick_us(&self) -> u64 {
        1_000_000 / u64::from(self.tick_rate_hz)
    }

    /// Adds `elapsed_ms` and returns how many ticks are due now.
    pub fn advance(&mut self, elapsed_ms: u64) -> u32 {
        self.accumulated_us = self
            .accumulated_us
            .saturating_add(elapsed_ms.saturating_mul(1000));
        let step = self.tick_us();
        let due = self.accumulated_us / step;
        let run = due.min(u64::from(self.max_catch_up));
        self.accumulated_us = if due > run {
            0
        } else {
            self.accumulated_us - run * step
        };
        run as u32
    }
}

/// Server-side driver: buffers per-peer input and advances the authoritative tick.
#[derive(Debug, Clone)]
pub struct TickDriver<I> {
    tick: u64,
    buffer_capacity: usize,
    buffers: BTreeMap<PeerId, InputBuffer<I>>,
    missed: BTreeMap<PeerId, u64>,
    history: VecDeque<(u64, TickInputs<I>)>,
    /// Spec 120 § 6.3: `merge(late, next)` for inputs that missed their tick (`None` = drop them).
    late_merge: Option<fn(I, I) -> I>,
    /// Recent ticks each peer missed (the last [`LATE_MERGE_TICKS`]): a late input for one of them
    /// is merged once.
    missed_ticks: BTreeMap<PeerId, BTreeSet<u64>>,
    /// Late inputs waiting for the next tick, merged in tick order.
    late: BTreeMap<PeerId, I>,
    /// Late inputs merged (`late_merged`).
    late_merged: BTreeMap<PeerId, u64>,
}

impl<I: Clone> TickDriver<I> {
    /// A driver at tick 0 whose per-peer buffers hold `buffer_capacity` ticks.
    pub fn new(buffer_capacity: usize) -> Self {
        Self {
            tick: 0,
            buffer_capacity: buffer_capacity.max(1),
            buffers: BTreeMap::new(),
            missed: BTreeMap::new(),
            history: VecDeque::with_capacity(TICK_HISTORY),
            late_merge: None,
            missed_ticks: BTreeMap::new(),
            late: BTreeMap::new(),
            late_merged: BTreeMap::new(),
        }
    }

    /// Spec 120 § 6.3: merges an input that arrives after its tick was simulated without it into
    /// the next tick the driver builds, as `merge(late, next)` when that tick has its own input
    /// (keep the presses, take the newest analog values) and as the late input itself when it does
    /// not. Only a tick the peer missed (within the last [`LATE_MERGE_TICKS`]) takes a late input,
    /// once; a duplicate or a late copy of an input already used is ignored. `None` (the default of [`TickDriver::new`]) drops late inputs.
    /// [`crate::NetServer`] sets [`NetCodec::merge_late`] of the game's input.
    pub fn set_late_merge(&mut self, merge: Option<fn(I, I) -> I>) {
        self.late_merge = merge;
        if merge.is_none() {
            self.late.clear();
        }
    }

    /// Late inputs of `peer` merged into a later tick since it joined.
    pub fn late_merged(&self, peer: PeerId) -> u64 {
        self.late_merged.get(&peer).copied().unwrap_or(0)
    }

    /// The next tick to simulate.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Moves an empty driver to `tick` (a lockstep host that resumes a migrated match, spec 120
    /// § 10.5): the next tick simulated is `tick`. Buffered inputs before it are dropped.
    pub(crate) fn resume_at(&mut self, tick: u64) {
        self.tick = tick;
        if let Some(before) = tick.checked_sub(1) {
            for buffer in self.buffers.values_mut() {
                buffer.take(before);
            }
        }
    }

    /// Starts collecting input for `peer`.
    pub fn add_peer(&mut self, peer: PeerId) {
        self.buffers
            .entry(peer)
            .or_insert_with(|| InputBuffer::new(self.buffer_capacity));
        self.missed.entry(peer).or_insert(0);
    }

    /// Stops collecting input for `peer`.
    pub fn remove_peer(&mut self, peer: PeerId) {
        self.buffers.remove(&peer);
        self.missed.remove(&peer);
        self.missed_ticks.remove(&peer);
        self.late.remove(&peer);
        self.late_merged.remove(&peer);
    }

    /// Peers registered.
    pub fn peers(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.buffers.keys().copied()
    }

    /// Buffers one input; returns whether it was stored. Unknown peers, past ticks and ticks
    /// further ahead than the buffer holds are ignored (an over-eager or hostile client cannot
    /// crowd out the inputs the server is about to use).
    /// With a late merge set, an input for a tick the peer missed is kept for the next tick
    /// instead (and returns true once).
    pub fn push_input(&mut self, peer: PeerId, tick: u64, input: I) -> bool {
        if tick < self.tick {
            return self.push_late(peer, tick, input);
        }
        if tick - self.tick >= self.buffer_capacity as u64 {
            return false;
        }
        self.buffers.get_mut(&peer).is_some_and(|b| b.insert(tick, input))
    }

    fn push_late(&mut self, peer: PeerId, tick: u64, input: I) -> bool {
        let Some(merge) = self.late_merge else {
            return false;
        };
        if !self.missed_ticks.get_mut(&peer).is_some_and(|m| m.remove(&tick)) {
            return false;
        }
        // An older late input is merged under a newer one; inputs arrive mostly in order, and a
        // newer tick's analog values win either way.
        let merged = match self.late.remove(&peer) {
            Some(prev) => merge(prev, input),
            None => input,
        };
        self.late.insert(peer, merged);
        *self.late_merged.entry(peer).or_insert(0) += 1;
        true
    }

    /// True when every registered peer has input waiting for `tick`.
    pub fn all_ready_for(&self, tick: u64) -> bool {
        !self.buffers.is_empty() && self.buffers.values().all(|b| b.has(tick))
    }

    /// True when `peer` is registered and has input waiting for `tick`. Reads only: nothing is
    /// taken or counted (ADR-7892).
    pub fn has_input(&self, peer: PeerId, tick: u64) -> bool {
        self.buffers.get(&peer).is_some_and(|b| b.has(tick))
    }

    /// True when every peer of `peers` has input waiting for `tick`: [`Self::all_ready_for`] over
    /// a set the game chooses, for example its live players while a cleanly leaving peer waits
    /// for its committed removal tick. Registered peers outside `peers` are ignored and keep their
    /// buffers; a peer of `peers` that is not registered is not ready. An empty set is not ready,
    /// as in [`Self::all_ready_for`]. Reads only (ADR-7892).
    pub fn ready_for<P: IntoIterator<Item = PeerId>>(&self, tick: u64, peers: P) -> bool {
        let mut any = false;
        for peer in peers {
            if !self.has_input(peer, tick) {
                return false;
            }
            any = true;
        }
        any
    }

    /// Advances one tick: collects the inputs for it, runs `step(tick, &inputs)`, records history
    /// and returns the inputs.
    pub fn advance<F: FnMut(u64, &TickInputs<I>)>(&mut self, mut step: F) -> TickInputs<I> {
        let current = self.tick;
        let mut inputs = TickInputs::new();
        let keep_from = current.saturating_sub(LATE_MERGE_TICKS);
        for (peer, buffer) in &mut self.buffers {
            let own = buffer.take(current);
            if own.is_none() {
                *self.missed.entry(*peer).or_insert(0) += 1;
                if self.late_merge.is_some() {
                    let m = self.missed_ticks.entry(*peer).or_default();
                    m.insert(current);
                    while m.first().is_some_and(|t| *t < keep_from) {
                        m.pop_first();
                    }
                }
            }
            let late = self.late.remove(peer);
            let input = match (late, own, self.late_merge) {
                (Some(late), Some(own), Some(merge)) => Some(merge(late, own)),
                (Some(late), None, _) => Some(late),
                (_, own, _) => own,
            };
            if let Some(input) = input {
                inputs.insert(*peer, input);
            }
        }
        if self.history.len() >= TICK_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back((current, inputs.clone()));
        step(current, &inputs);
        self.tick = current + 1;
        inputs
    }

    /// Ticks `peer` missed since it joined.
    pub fn missed(&self, peer: PeerId) -> u64 {
        self.missed.get(&peer).copied().unwrap_or(0)
    }

    /// The inputs used for a recent tick (the last [`TICK_HISTORY`]).
    pub fn inputs_at(&self, tick: u64) -> Option<&TickInputs<I>> {
        self.history.iter().find(|(t, _)| *t == tick).map(|(_, i)| i)
    }
}

/// A run of consecutive inputs, newest last: `first_tick u64 | count u8 | input*`.
#[derive(Debug, Clone, PartialEq)]
pub struct InputBundle<I> {
    /// Tick of the first input.
    pub first_tick: u64,
    /// Inputs for `first_tick`, `first_tick + 1`, ...
    pub inputs: Vec<I>,
}

impl<I: NetCodec> InputBundle<I> {
    /// Encodes the bundle.
    pub fn encode(&self, w: &mut WireWriter) {
        let count = self.inputs.len().min(MAX_BUNDLE_INPUTS);
        // Only the newest `count` inputs fit: stamp the first one that is written.
        let skipped = (self.inputs.len() - count) as u64;
        w.u64(self.first_tick.saturating_add(skipped));
        w.u8(count as u8);
        // The first input in full, each next one against the one before (`NetCodec::encode_delta`,
        // a full value unless the game codes deltas; ADR-8312).
        let mut previous: Option<&I> = None;
        for input in &self.inputs[self.inputs.len() - count..] {
            match previous {
                Some(p) => input.encode_delta(p, w),
                None => input.encode(w),
            }
            previous = Some(input);
        }
    }

    /// Decodes a bundle (untrusted bytes).
    pub fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let first_tick = r.u64()?;
        let count = usize::from(r.u8()?);
        if count > MAX_BUNDLE_INPUTS {
            return Err(WireError::TooLong {
                len: count,
                max: MAX_BUNDLE_INPUTS,
            });
        }
        let mut inputs: Vec<I> = Vec::with_capacity(count);
        for _ in 0..count {
            let input = match inputs.last() {
                Some(p) => I::decode_delta(p, r)?,
                None => I::decode(r)?,
            };
            inputs.push(input);
        }
        Ok(Self { first_tick, inputs })
    }

    /// Tick of the last (newest) input; `None` for an empty bundle.
    pub fn last_tick(&self) -> Option<u64> {
        let n = self.inputs.len().checked_sub(1)?;
        Some(self.first_tick.saturating_add(n as u64))
    }

    /// `(tick, input)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &I)> {
        self.inputs
            .iter()
            .enumerate()
            .map(|(i, input)| (self.first_tick.saturating_add(i as u64), input))
    }
}

/// How far behind the newest input a remembered one may be, in ticks (the server's default input
/// buffer): older inputs can no longer matter, and a push this far behind the newest means the
/// tick numbering restarted (ADR-8312).
pub const MAX_UNACKED_INPUTS: usize = 64;

/// The residual input loss loss-aware redundancy aims at (spec 120 § 6.1 item 3): 0.1 %.
pub const RESIDUAL_INPUT_LOSS: f64 = 0.001;

/// Datagrams each input should ride in so that, at independent loss `loss`, it is lost with
/// probability at most [`RESIDUAL_INPUT_LOSS`]: `ceil(log(0.001) / log(loss))` (spec 120 § 6.1
/// item 3, ADR-8312). `loss` is clamped to `[0.001, 0.5]`, so the count is 1 to 10.
pub fn loss_aware_copies(loss: f64) -> u8 {
    let p = if loss.is_finite() {
        loss.clamp(0.001, 0.5)
    } else {
        0.5
    };
    (RESIDUAL_INPUT_LOSS.ln() / p.ln()).ceil().clamp(1.0, 10.0) as u8
}

/// One remembered input: its tick, the input and the datagrams it rode in so far.
#[derive(Debug, Clone)]
pub(crate) struct Remembered<I> {
    tick: u64,
    input: I,
    copies: u8,
}

/// Client side: remembers recent inputs and builds redundant bundles from them.
///
/// It keeps the newest unacknowledged inputs (spec 120 § 6.1 item 1, unacked-only redundancy;
/// ADR-8312): an input the server acknowledged ([`InputHistory::ack_through`]) is dropped and never
/// sent again, and at most `redundancy` inputs are kept, or the loss-aware copy count when that is
/// larger (`NetClient` sets it from the loss it measures). The `NetClient` default redundancy is
/// [`MAX_BUNDLE_INPUTS`], so every unacknowledged input rides along while one bundle holds it. A
/// tick that does not follow the newest one (a skipped tick, a repeated one, an older one) no
/// longer restarts the history: the inputs are kept sorted by tick, a repeated tick keeps its
/// first input, and bundles are cut into runs of consecutive ticks.
#[derive(Debug, Clone)]
pub struct InputHistory<I> {
    redundancy: usize,
    /// The loss-aware copy count: keeps at least this many inputs when `redundancy` is lower.
    copies_floor: usize,
    /// Sorted by tick, no duplicates.
    recent: VecDeque<Remembered<I>>,
    /// Newest tick the server acknowledged: every input up to it arrived or no longer matters.
    acked_through: Option<u64>,
}

impl<I: Clone> InputHistory<I> {
    /// A history repeating the last `redundancy` unacknowledged inputs (1..=[`MAX_BUNDLE_INPUTS`]).
    pub fn new(redundancy: usize) -> Self {
        Self {
            redundancy: redundancy.clamp(1, MAX_BUNDLE_INPUTS),
            copies_floor: 1,
            recent: VecDeque::new(),
            acked_through: None,
        }
    }

    /// Sets the loss-aware copy count ([`loss_aware_copies`] of the measured loss): the history
    /// keeps at least that many inputs even when `redundancy` is lower, so each input rides in
    /// that many datagrams (spec 120 § 6.1 item 3). Takes effect from the next push.
    pub(crate) fn set_copies_floor(&mut self, copies: u8) {
        self.copies_floor = usize::from(copies).clamp(1, MAX_BUNDLE_INPUTS);
    }

    /// Inputs kept at most.
    fn capacity(&self) -> usize {
        self.redundancy.max(self.copies_floor)
    }

    /// Records the input for `tick`. A tick already remembered keeps its first input (every copy
    /// the server may receive agrees); a tick at or before the acknowledged one is dropped; a tick
    /// further than [`MAX_UNACKED_INPUTS`] before the newest starts the history over (the tick
    /// numbering restarted).
    pub fn push(&mut self, tick: u64, input: I) {
        let _ = self.push_undoable(tick, input);
    }

    /// The server acknowledged every input up to `tick` (its newest contiguous input tick, spec
    /// 120 § 6.4): those inputs are dropped and are never sent again. An older acknowledgment
    /// than one already taken changes nothing.
    pub fn ack_through(&mut self, tick: u64) {
        if self.acked_through.is_some_and(|a| a >= tick) {
            return;
        }
        self.acked_through = Some(tick);
        while self.recent.front().is_some_and(|r| r.tick <= tick) {
            self.recent.pop_front();
        }
    }

    /// The newest tick the server acknowledged, if any.
    pub fn acked_through(&self) -> Option<u64> {
        self.acked_through
    }

    /// The bundle of the newest run of consecutive inputs (at most [`MAX_BUNDLE_INPUTS`]), or
    /// `None` before the first input.
    pub fn bundle(&self) -> Option<InputBundle<I>> {
        let end = self.recent.len();
        let mut start = end.checked_sub(1)?;
        while start > 0
            && end - start < MAX_BUNDLE_INPUTS
            && self.recent[start - 1].tick.checked_add(1) == Some(self.recent[start].tick)
        {
            start -= 1;
        }
        Some(InputBundle {
            first_tick: self.recent[start].tick,
            inputs: self.recent.range(start..end).map(|r| r.input.clone()).collect(),
        })
    }

    /// Inputs remembered.
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    /// True before the first input (or once every input is acknowledged).
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }

    /// The newest remembered tick.
    pub fn newest_tick(&self) -> Option<u64> {
        self.recent.back().map(|r| r.tick)
    }

    /// The remembered input of `tick`.
    pub(crate) fn get(&self, tick: u64) -> Option<&I> {
        let i = self.recent.binary_search_by_key(&tick, |r| r.tick).ok()?;
        self.recent.get(i).map(|r| &r.input)
    }

    /// [`InputHistory::push`], returning what it changed so [`InputHistory::undo`] can restore
    /// the history exactly (ADR-7896: a sample whose send failed must not stay in the history).
    pub(crate) fn push_undoable(&mut self, tick: u64, input: I) -> PushUndo<I> {
        if self.acked_through.is_some_and(|a| tick <= a) {
            return PushUndo::Nothing;
        }
        let far_back = self
            .newest_tick()
            .is_some_and(|n| n > tick && n - tick > MAX_UNACKED_INPUTS as u64);
        if far_back {
            let old = std::mem::take(&mut self.recent);
            let acked = self.acked_through.take();
            self.recent.push_back(Remembered {
                tick,
                input,
                copies: 0,
            });
            return PushUndo::Restarted(old, acked);
        }
        let pos = match self.recent.binary_search_by_key(&tick, |r| r.tick) {
            Ok(_) => return PushUndo::Nothing,
            Err(pos) => pos,
        };
        self.recent.insert(
            pos,
            Remembered {
                tick,
                input,
                copies: 0,
            },
        );
        // Over capacity, or too old to matter to the server any more: the oldest go.
        let mut evicted = Vec::new();
        while self.recent.len() > self.capacity()
            || self.recent.front().is_some_and(|r| {
                self.newest_tick()
                    .is_some_and(|n| n - r.tick >= MAX_UNACKED_INPUTS as u64)
            })
        {
            match self.recent.pop_front() {
                Some(r) => evicted.push(r),
                None => break,
            }
        }
        PushUndo::Inserted(tick, evicted)
    }

    /// Takes back the push that returned `undo` (it must be the latest change).
    pub(crate) fn undo(&mut self, undo: PushUndo<I>) {
        match undo {
            PushUndo::Nothing => {}
            PushUndo::Restarted(old, acked) => {
                self.recent = old;
                self.acked_through = acked;
            }
            PushUndo::Inserted(tick, evicted) => {
                if let Ok(i) = self.recent.binary_search_by_key(&tick, |r| r.tick) {
                    self.recent.remove(i);
                }
                for r in evicted.into_iter().rev() {
                    self.recent.push_front(r);
                }
            }
        }
    }
}

/// What one [`InputHistory`] push changed.
#[derive(Debug)]
pub(crate) enum PushUndo<I> {
    /// Nothing (a repeated or acknowledged tick).
    Nothing,
    /// The tick numbering restarted: the whole previous history and its acknowledgment.
    Restarted(VecDeque<Remembered<I>>, Option<u64>),
    /// The tick was inserted; the oldest inputs it pushed out, oldest first.
    Inserted(u64, Vec<Remembered<I>>),
}

/// Bytes of an encoded [`InputBundle`] in front of its inputs: `first_tick u64 | count u8`.
pub const BUNDLE_HEADER_BYTES: usize = 9;

/// Largest encoded input the reliable input lane carries (ADR-7896): an input too large for one
/// datagram travels reliably, fragmented by the reliable channel, as `tick u64 | input`. A
/// protocol constant both ends agree on; a server refuses a longer one as hostile input. With
/// the default 64-tick input buffer one peer can hold at most 1 MiB of such inputs on the server.
pub const MAX_RELIABLE_INPUT_BYTES: usize = 16 * 1024;

/// Encodes `tick u64 | input` for the reliable input lane, or fails with
/// [`crate::SendError::TooLarge`] when the input encodes longer than [`MAX_RELIABLE_INPUT_BYTES`]
/// (or not at all). `lane` is written first.
pub(crate) fn encode_reliable_input<I: NetCodec>(
    lane: u8,
    tick: u64,
    input: &I,
) -> Result<Vec<u8>, crate::connection::SendError> {
    let mut body = WireWriter::new();
    input.encode(&mut body);
    let len = body.len();
    let too_large = crate::connection::SendError::TooLarge {
        len,
        max: MAX_RELIABLE_INPUT_BYTES,
    };
    let body = body.finish_checked().map_err(|_| too_large.clone())?;
    if body.len() > MAX_RELIABLE_INPUT_BYTES {
        return Err(too_large);
    }
    let mut w = WireWriter::with_capacity(1 + 8 + body.len());
    w.u8(lane);
    w.u64(tick);
    w.raw(&body);
    Ok(w.finish())
}

/// Decodes the body of a reliable input message (after the lane byte; untrusted bytes). The
/// length is checked before the input decoder runs: an input longer than
/// [`MAX_RELIABLE_INPUT_BYTES`] is [`WireError::TooLong`] (a protocol violation, ADR-7892), and
/// bytes left after the input are an error.
pub(crate) fn decode_reliable_input<I: NetCodec>(r: &mut WireReader<'_>) -> Result<(u64, I), WireError> {
    let tick = r.u64()?;
    if r.remaining() > MAX_RELIABLE_INPUT_BYTES {
        return Err(WireError::TooLong {
            len: r.remaining(),
            max: MAX_RELIABLE_INPUT_BYTES,
        });
    }
    let input = I::decode(r)?;
    r.finish()?;
    Ok((tick, input))
}

/// The encoded length of `value`, or `None` when its encoding fails.
fn encoded_len<I: NetCodec>(value: &I) -> Option<usize> {
    let mut w = WireWriter::new();
    value.encode(&mut w);
    w.finish_checked().ok().map(|b| b.len())
}

/// Datagrams one input send may use for its unreliable bundles (ADR-7896). The first carries the
/// newest inputs; when the remembered history does not fit one datagram, the older inputs follow
/// in up to this many minus one more, so large inputs keep their redundancy under loss.
pub const MAX_INPUT_BUNDLES_PER_SEND: usize = 4;

impl<I: Clone + NetCodec> InputHistory<I> {
    /// The bundle of the newest consecutive inputs whose encoding fits `max_bytes` (header
    /// included): it always ends at the newest input, and holds as many older ones as fit
    /// (ADR-7896). `None` when even the newest input alone does not fit, or before the first
    /// input. Small inputs get the whole history, as [`InputHistory::bundle`] gives.
    pub fn bundle_within(&self, max_bytes: usize) -> Option<InputBundle<I>> {
        let newest = self.newest_tick()?;
        let bundle = self.bundles_within(max_bytes, 1).into_iter().next()?;
        (bundle.last_tick() == Some(newest)).then_some(bundle)
    }

    /// The remembered history cut into at most `max_bundles` bundles of consecutive inputs, each
    /// encoding to at most `max_bytes`, newest first (ADR-7896). When the newest input fits, the
    /// first bundle is [`InputHistory::bundle_within`]; each next one holds the inputs just older
    /// than the one before (a gap in the ticks also ends a bundle). An input too large for a
    /// bundle on its own (it travels on the reliable input lane) is skipped, so the inputs older
    /// than it keep their redundancy.
    pub fn bundles_within(&self, max_bytes: usize, max_bundles: usize) -> Vec<InputBundle<I>> {
        self.cut_bundles(max_bytes, max_bundles, u8::MAX)
            .into_iter()
            .map(|(start, end)| self.bundle_of(start, end))
            .collect()
    }

    /// The bundles one send carries (spec 120 § 6.1, ADR-8312): the first holds the newest
    /// consecutive inputs that fit `max_bytes`; each further one (at most `max_bundles` in all) is
    /// sent only when it holds an input that rode in fewer than `min_copies` datagrams so far
    /// ([`loss_aware_copies`]). The inputs of the returned bundles count one more copy.
    pub(crate) fn bundles_for_send(
        &mut self,
        max_bytes: usize,
        max_bundles: usize,
        min_copies: u8,
    ) -> Vec<InputBundle<I>> {
        let ranges = self.cut_bundles(max_bytes, max_bundles, min_copies);
        let bundles = ranges
            .iter()
            .map(|&(start, end)| self.bundle_of(start, end))
            .collect();
        for (start, end) in ranges {
            for r in self.recent.range_mut(start..end) {
                r.copies = r.copies.saturating_add(1);
            }
        }
        bundles
    }

    fn bundle_of(&self, start: usize, end: usize) -> InputBundle<I> {
        InputBundle {
            first_tick: self.recent.get(start).map_or(0, |r| r.tick),
            inputs: self.recent.range(start..end).map(|r| r.input.clone()).collect(),
        }
    }

    /// `(start, end)` index ranges of the bundles, newest first. Bundles after the first are kept
    /// only when one of their inputs has fewer than `min_copies` copies.
    fn cut_bundles(&self, max_bytes: usize, max_bundles: usize, min_copies: u8) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        // Index one past the newest input not yet in a bundle.
        let mut end = self.recent.len();
        while end > 0 && ranges.len() < max_bundles {
            let mut used = BUNDLE_HEADER_BYTES;
            let mut start = end;
            while start > 0 && end - start < MAX_BUNDLE_INPUTS {
                // A gap in the ticks ends the bundle: it carries consecutive inputs only.
                if start < end && self.recent[start - 1].tick.checked_add(1) != Some(self.recent[start].tick)
                {
                    break;
                }
                // An input whose encoding fails (a field over its length prefix) never fits.
                let Some(len) = self.recent.get(start - 1).and_then(|r| encoded_len(&r.input)) else {
                    break;
                };
                if used.saturating_add(len) > max_bytes {
                    break;
                }
                used += len;
                start -= 1;
            }
            if start == end {
                end -= 1; // No bundle holds this input: skip it.
                continue;
            }
            let wanted = ranges.is_empty() || self.recent.range(start..end).any(|r| r.copies < min_copies);
            if wanted {
                ranges.push((start, end));
            }
            end = start;
        }
        ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_accepts_out_of_order_and_rejects_stale() {
        let mut b = InputBuffer::new(4);
        assert!(b.insert(5, 'e'));
        assert!(b.insert(3, 'c'));
        assert!(!b.insert(5, 'x'), "duplicate");
        assert_eq!(b.take(3), Some('c'));
        assert!(!b.insert(2, 'b'), "already simulated");
        assert_eq!(b.take(4), None, "never arrived");
        assert_eq!(b.take(5), Some('e'));
    }

    #[test]
    fn full_buffer_evicts_oldest_not_newest() {
        let mut b = InputBuffer::new(2);
        assert!(b.insert(10, 1));
        assert!(b.insert(11, 2));
        assert!(!b.insert(9, 0), "older than everything held");
        assert!(b.insert(12, 3));
        assert_eq!(b.first_tick(), Some(11));
    }

    #[test]
    fn driver_collects_in_peer_order_and_counts_misses() {
        let mut d: TickDriver<u32> = TickDriver::new(8);
        d.add_peer(PeerId(2));
        d.add_peer(PeerId(1));
        d.push_input(PeerId(1), 0, 10);
        d.push_input(PeerId(2), 0, 20);
        d.push_input(PeerId(1), 1, 11);
        assert!(d.all_ready_for(0));
        assert!(!d.all_ready_for(1));
        let mut seen = Vec::new();
        d.advance(|tick, inputs| {
            seen.push((tick, inputs.iter().map(|(p, i)| (p.0, *i)).collect::<Vec<_>>()))
        });
        d.advance(|tick, inputs| {
            seen.push((tick, inputs.iter().map(|(p, i)| (p.0, *i)).collect::<Vec<_>>()))
        });
        assert_eq!(seen, vec![(0, vec![(1, 10), (2, 20)]), (1, vec![(1, 11)])]);
        assert_eq!(d.missed(PeerId(2)), 1);
        assert_eq!(d.tick(), 2);
        assert_eq!(d.inputs_at(0).map(|i| i.len()), Some(2));
        assert!(!d.push_input(PeerId(1), 1, 99), "past tick refused");
    }

    /// DO27's case (ADR-7892): A and B are registered, A sent tick 0, B is leaving cleanly and its
    /// removal tick is not reached yet. The live set [A] is ready; the full set is not; nothing is
    /// consumed by asking.
    #[test]
    fn selected_set_readiness_ignores_a_leaving_peer() {
        let (a, b) = (PeerId(1), PeerId(2));
        let mut d: TickDriver<u32> = TickDriver::new(8);
        d.add_peer(a);
        d.add_peer(b);
        assert!(d.push_input(a, 0, 7));
        assert!(!d.all_ready_for(0), "B has nothing for tick 0");
        assert!(d.ready_for(0, [a]), "the live set [A] is ready");
        assert!(!d.ready_for(0, [a, b]));
        assert!(!d.ready_for(0, [b]));
        assert!(d.has_input(a, 0));
        assert!(!d.has_input(b, 0));
        assert!(!d.has_input(a, 1), "nothing for tick 1 yet");
        assert!(!d.ready_for(1, [a]));
        for _ in 0..3 {
            assert!(d.ready_for(0, vec![a]), "asking is repeatable");
        }
        assert_eq!(d.missed(a), 0, "asking counts no miss");
        assert_eq!(d.missed(b), 0);
        let inputs = d.advance(|_, _| {});
        assert_eq!(inputs.get(&a), Some(&7), "asking consumed nothing");
        assert!(!inputs.contains_key(&b));
        assert_eq!(d.missed(b), 1, "B is still registered until the game removes it");
        assert!(!d.has_input(a, 0), "tick 0 was consumed");
    }

    #[test]
    fn selected_set_readiness_edges() {
        let mut d: TickDriver<u32> = TickDriver::new(8);
        assert!(!d.ready_for(0, []), "an empty set is not ready");
        assert!(!d.has_input(PeerId(9), 0), "unknown peer");
        d.add_peer(PeerId(1));
        assert!(
            !d.ready_for(0, []),
            "an empty set is not ready, like all_ready_for"
        );
        assert!(!d.ready_for(0, [PeerId(9)]), "an unregistered peer is not ready");
        assert!(d.push_input(PeerId(1), 0, 1));
        assert!(!d.ready_for(0, [PeerId(1), PeerId(9)]));
        assert!(d.ready_for(0, [PeerId(1), PeerId(1)]), "duplicates are fine");
        d.remove_peer(PeerId(1));
        assert!(!d.has_input(PeerId(1), 0), "removal drops the buffer");
        assert!(!d.ready_for(0, [PeerId(1)]));
    }

    #[test]
    fn redundant_bundles_heal_a_lost_packet() {
        let mut client = InputHistory::new(3);
        let mut server: TickDriver<u32> = TickDriver::new(16);
        server.add_peer(PeerId(1));
        for tick in 0..10u64 {
            client.push(tick, tick as u32 * 7);
            let bundle = client.bundle().expect("non-empty");
            let mut w = WireWriter::new();
            bundle.encode(&mut w);
            let bytes = w.finish();
            if tick == 4 || tick == 5 {
                continue; // two packets in a row lost
            }
            let back = InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).expect("valid");
            for (t, input) in back.iter() {
                server.push_input(PeerId(1), t, *input);
            }
        }
        for _ in 0..10 {
            server.advance(|_, _| {});
        }
        assert_eq!(
            server.missed(PeerId(1)),
            0,
            "redundancy 3 covers two lost packets"
        );
    }

    #[test]
    fn oversized_bundle_keeps_the_newest_inputs_at_their_ticks() {
        let bundle = InputBundle {
            first_tick: 100,
            inputs: (0..40u32).collect(),
        };
        let mut w = WireWriter::new();
        bundle.encode(&mut w);
        let bytes = w.finish();
        let back = InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).expect("valid");
        let pairs: Vec<(u64, u32)> = back.iter().map(|(t, i)| (t, *i)).collect();
        assert_eq!(pairs.len(), MAX_BUNDLE_INPUTS);
        assert_eq!(pairs[0], (108, 8));
        assert_eq!(pairs[31], (139, 39));
    }

    #[test]
    fn far_future_inputs_cannot_crowd_out_current_ones() {
        let mut d: TickDriver<u32> = TickDriver::new(8);
        d.add_peer(PeerId(1));
        for tick in 1_000..1_100 {
            assert!(!d.push_input(PeerId(1), tick, 0), "beyond the buffer horizon");
        }
        assert!(
            !d.push_input(PeerId(1), 8, 0),
            "exactly one buffer ahead is refused"
        );
        assert!(d.push_input(PeerId(1), 0, 42));
        let inputs = d.advance(|_, _| {});
        assert_eq!(inputs.get(&PeerId(1)), Some(&42));
    }

    #[test]
    fn hostile_bundle_is_an_error() {
        let mut w = WireWriter::new();
        w.u64(0);
        w.u8(200);
        let bytes = w.finish();
        assert!(InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).is_err());
        let mut w = WireWriter::new();
        w.u64(0);
        w.u8(3);
        w.u32(1);
        let bytes = w.finish();
        assert!(
            InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).is_err(),
            "truncated"
        );
    }

    /// A sample of `n` payload bytes (`Vec<u8>` encodes as `len u16 | bytes`).
    fn sample(n: usize, tag: u8) -> Vec<u8> {
        vec![tag; n]
    }

    fn encoded(bundle: &InputBundle<Vec<u8>>) -> usize {
        let mut w = WireWriter::new();
        bundle.encode(&mut w);
        w.finish().len()
    }

    #[test]
    fn small_inputs_keep_the_whole_history_in_one_bundle() {
        let mut h = InputHistory::new(15);
        for tick in 0..20u64 {
            h.push(tick, sample(8, tick as u8));
        }
        let full = h.bundle().expect("inputs");
        assert_eq!(
            h.bundle_within(1153),
            Some(full.clone()),
            "same as the unbounded bundle"
        );
        let all = h.bundles_within(1153, MAX_INPUT_BUNDLES_PER_SEND);
        assert_eq!(all, vec![full], "one datagram, no extra bundles");
    }

    #[test]
    fn large_inputs_split_newest_first_within_the_budget() {
        // The DO27 shape: a 548-byte sample, 15 remembered, a 1153-byte bundle budget.
        let mut h = InputHistory::new(15);
        for tick in 0..15u64 {
            h.push(100 + tick, sample(546, tick as u8));
        }
        let newest = h.bundle_within(1153).expect("one fits");
        assert_eq!(newest.inputs.len(), 2, "9 + 2 * 548 fits, 9 + 3 * 548 does not");
        assert_eq!(newest.first_tick, 113);
        assert!(encoded(&newest) <= 1153);
        let bundles = h.bundles_within(1153, MAX_INPUT_BUNDLES_PER_SEND);
        assert_eq!(bundles.len(), MAX_INPUT_BUNDLES_PER_SEND);
        let firsts: Vec<u64> = bundles.iter().map(|b| b.first_tick).collect();
        assert_eq!(firsts, vec![113, 111, 109, 107], "consecutive runs, newest first");
        for b in &bundles {
            assert!(encoded(b) <= 1153, "every bundle fits");
            for (tick, input) in b.iter() {
                assert_eq!(input[0], (tick - 100) as u8, "each input at its own tick");
            }
        }
    }

    #[test]
    fn an_input_larger_than_the_budget_is_skipped_not_a_wall() {
        let mut h = InputHistory::new(15);
        h.push(0, sample(8, 0));
        h.push(1, sample(4000, 1));
        assert_eq!(h.bundle_within(1153), None, "too large on its own");
        let older = h.bundles_within(1153, MAX_INPUT_BUNDLES_PER_SEND);
        assert_eq!(older.len(), 1, "the inputs before it still go");
        assert_eq!((older[0].first_tick, older[0].inputs.len()), (0, 1));
        h.push(2, sample(8, 2));
        let bundles = h.bundles_within(1153, MAX_INPUT_BUNDLES_PER_SEND);
        let runs: Vec<(u64, usize)> = bundles.iter().map(|b| (b.first_tick, b.inputs.len())).collect();
        assert_eq!(
            runs,
            vec![(2, 1), (0, 1)],
            "newest first, the large input skipped"
        );
        assert_eq!(h.bundle_within(1153).map(|b| b.first_tick), Some(2));
        assert_eq!(InputHistory::<Vec<u8>>::new(4).bundle_within(1153), None, "empty");
    }

    #[test]
    fn undo_restores_the_history_exactly() {
        let mut h = InputHistory::new(3);
        for tick in 0..3u64 {
            h.push(tick, tick as u32);
        }
        let before = h.bundle();
        let undo = h.push_undoable(3, 3);
        assert_eq!(
            h.bundle().map(|b| b.first_tick),
            Some(1),
            "the oldest was evicted"
        );
        h.undo(undo);
        assert_eq!(h.bundle(), before, "eviction undone");
        // ADR-8312: a gap keeps the older run (the oldest input is evicted for room).
        let undo = h.push_undoable(9, 9);
        assert_eq!(h.len(), 3, "a gap keeps the run");
        assert_eq!(h.bundle().map(|b| b.first_tick), Some(9));
        h.undo(undo);
        assert_eq!(h.bundle(), before, "gap push undone");
        assert_eq!(h.len(), 3);
        // A far older tick (a new numbering) restarts it; undo restores it.
        let mut far = InputHistory::new(3);
        far.push(1_000, 1u32);
        far.push(1_001, 2);
        let kept = far.bundle();
        let undo = far.push_undoable(5, 5);
        assert_eq!(far.len(), 1, "a new numbering restarted the history");
        far.undo(undo);
        assert_eq!(far.bundle(), kept, "restart undone");
        let undo = h.push_undoable(3, 3);
        h.undo(undo);
        h.push(3, 3);
        assert_eq!(h.newest_tick(), Some(3), "the next push still extends the run");
        assert_eq!(h.bundle().map(|b| b.inputs), Some(vec![1, 2, 3]));
    }

    #[test]
    fn reliable_input_round_trips_and_refuses_hostile_bytes() {
        let bytes = encode_reliable_input(3, 77, &sample(4096, 5)).expect("fits");
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.u8(), Ok(3));
        let (tick, input) = decode_reliable_input::<Vec<u8>>(&mut r).expect("valid");
        assert_eq!((tick, input), (77, sample(4096, 5)));
        assert!(matches!(
            encode_reliable_input(3, 0, &sample(MAX_RELIABLE_INPUT_BYTES, 0)),
            Err(crate::connection::SendError::TooLarge { .. })
        ));
        // Over the limit: refused before the input decoder runs.
        let mut w = WireWriter::new();
        w.u64(1);
        w.raw(&vec![0u8; MAX_RELIABLE_INPUT_BYTES + 1]);
        let over = w.finish();
        assert_eq!(
            decode_reliable_input::<Vec<u8>>(&mut WireReader::new(&over)).map(|_| ()),
            Err(WireError::TooLong {
                len: MAX_RELIABLE_INPUT_BYTES + 1,
                max: MAX_RELIABLE_INPUT_BYTES
            })
        );
        // Truncated tick, truncated input, trailing bytes: malformed, not over the cap.
        for bad in [
            vec![1u8, 2, 3],
            {
                let mut w = WireWriter::new();
                w.u64(1);
                w.u16(2);
                w.finish()
            },
            {
                let mut w = WireWriter::new();
                w.u64(1);
                w.u32(5);
                w.u8(9);
                w.finish()
            },
        ] {
            let got = decode_reliable_input::<u32>(&mut WireReader::new(&bad)).map(|_| ());
            assert!(got.is_err(), "{bad:?}");
            assert!(!matches!(got, Err(WireError::TooLong { .. })), "{got:?}");
        }
    }

    /// A per-tick input with counted presses and an analog value (spec 120 § 6.3).
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    struct Pad {
        x: i8,
        presses: u8,
    }

    impl NetCodec for Pad {
        fn encode(&self, w: &mut WireWriter) {
            w.u8(self.x as u8);
            w.u8(self.presses);
        }
        fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
            Ok(Self {
                x: r.u8()? as i8,
                presses: r.u8()?,
            })
        }

        fn merge_late(late: Self, next: Self) -> Self {
            merge_pad(late, next)
        }
    }

    fn merge_pad(late: Pad, next: Pad) -> Pad {
        Pad {
            x: next.x,
            presses: late.presses.saturating_add(next.presses),
        }
    }

    #[test]
    fn a_late_input_merges_into_the_next_tick_once() {
        let mut d: TickDriver<Pad> = TickDriver::new(8);
        d.set_late_merge(Some(merge_pad));
        d.add_peer(PeerId(1));
        d.push_input(PeerId(1), 0, Pad { x: 1, presses: 0 });
        d.advance(|_, _| {});
        // Tick 1 runs without the peer's input; it arrives later with a press, twice.
        d.advance(|_, _| {});
        assert_eq!(d.missed(PeerId(1)), 1);
        assert!(
            d.push_input(PeerId(1), 1, Pad { x: 2, presses: 1 }),
            "late, merged"
        );
        assert!(
            !d.push_input(PeerId(1), 1, Pad { x: 2, presses: 1 }),
            "a duplicate merges nothing"
        );
        assert!(
            !d.push_input(PeerId(1), 0, Pad { x: 9, presses: 1 }),
            "tick 0 was not missed"
        );
        d.push_input(PeerId(1), 2, Pad { x: 3, presses: 1 });
        let inputs = d.advance(|_, _| {});
        assert_eq!(
            inputs.get(&PeerId(1)),
            Some(&Pad { x: 3, presses: 2 }),
            "both presses kept, the newest analog value"
        );
        assert_eq!(d.late_merged(PeerId(1)), 1);
        // A late input for a tick that has no input of its own is used as it is.
        d.advance(|_, _| {});
        assert!(d.push_input(PeerId(1), 3, Pad { x: 4, presses: 1 }));
        let inputs = d.advance(|_, _| {});
        assert_eq!(inputs.get(&PeerId(1)), Some(&Pad { x: 4, presses: 1 }));
    }

    #[test]
    fn without_a_late_merge_late_inputs_are_dropped() {
        let mut d: TickDriver<Pad> = TickDriver::new(8);
        d.add_peer(PeerId(1));
        d.advance(|_, _| {});
        assert!(!d.push_input(PeerId(1), 0, Pad { x: 1, presses: 1 }));
        assert_eq!(d.advance(|_, _| {}).get(&PeerId(1)), None);
        assert_eq!(d.late_merged(PeerId(1)), 0);
    }

    /// The DO27 scenario on the `TickDriver` path (spec 120 § 6.3): four peers at 0, 6, 12 and 18
    /// ticks (0-300 ms at 60 Hz) plus 0-2 ticks of jitter, Gilbert-Elliott loss bursts, 5 %
    /// duplicates and a one-second blackout whose packets are queued and flushed. Each client
    /// stamps its input a fixed lead ahead and repeats its last [`MAX_BUNDLE_INPUTS`] inputs per
    /// packet; presses come one tick at a time. Every press that reached the server in some copy
    /// reaches the simulation exactly once, and with that redundancy every press reaches the server.
    #[test]
    fn late_inputs_under_latency_bursts_a_blackout_and_duplicates_keep_every_press() {
        const TICKS: u64 = 6_000;
        const LEAD: u64 = 3;
        for seed in [7u64, 8, 9] {
            let mut rng = crate::testing::SplitMix64::new(seed);
            let mut d: TickDriver<Pad> = TickDriver::new(64);
            d.set_late_merge(Some(<Pad as NetCodec>::merge_late));
            let peers: Vec<PeerId> = (1..=4).map(PeerId).collect();
            let mut histories: Vec<InputHistory<Pad>> = peers
                .iter()
                .map(|_| InputHistory::new(MAX_BUNDLE_INPUTS))
                .collect();
            let mut issued = [0u64; 4];
            // Presses (peer index, tick) the server received in at least one copy.
            let mut arrived: BTreeSet<(usize, u64)> = BTreeSet::new();
            let mut applied = [0u64; 4];
            let mut bad = [false; 4];
            // (arrival tick, peer index, bundle)
            let mut in_flight: Vec<(u64, usize, InputBundle<Pad>)> = Vec::new();
            for p in &peers {
                d.add_peer(*p);
            }
            for now in 0..TICKS {
                // Each client builds the input for `now + LEAD` and sends its bundle.
                for (i, h) in histories.iter_mut().enumerate() {
                    let press = now > 60 && now + 200 < TICKS && rng.next_u64().is_multiple_of(12);
                    issued[i] += u64::from(press);
                    h.push(
                        now + LEAD,
                        Pad {
                            x: (now % 7) as i8,
                            presses: u8::from(press),
                        },
                    );
                    bad[i] = if bad[i] {
                        rng.next_f64() >= 0.3
                    } else {
                        rng.next_f64() < 0.01
                    };
                    let loss = if bad[i] { 0.6 } else { 0.01 };
                    if rng.next_f64() < loss {
                        continue;
                    }
                    let Some(bundle) = h.bundle() else { continue };
                    let latency = 6 * i as u64 + rng.next_u64() % 3;
                    let blackout = if (3_000..3_060).contains(&now) {
                        3_060 - now
                    } else {
                        0
                    };
                    let at = now + latency + blackout;
                    if rng.next_f64() < 0.05 {
                        in_flight.push((at + 1, i, bundle.clone()));
                    }
                    in_flight.push((at, i, bundle));
                }
                // Deliver what is due, then simulate the tick.
                in_flight.sort_by_key(|(at, i, _)| (*at, *i));
                let due: Vec<(u64, usize, InputBundle<Pad>)> = in_flight
                    .iter()
                    .filter(|(at, _, _)| *at <= now)
                    .cloned()
                    .collect();
                in_flight.retain(|(at, _, _)| *at > now);
                for (_, i, bundle) in due {
                    for (tick, input) in bundle.iter() {
                        if input.presses > 0 {
                            arrived.insert((i, tick));
                        }
                        d.push_input(peers[i], tick, *input);
                    }
                }
                let inputs = d.advance(|_, _| {});
                for (i, p) in peers.iter().enumerate() {
                    applied[i] += inputs.get(p).map_or(0, |pad| u64::from(pad.presses));
                }
            }
            for i in 0..4 {
                let received = arrived.iter().filter(|(p, _)| *p == i).count() as u64;
                assert!(issued[i] > 300, "seed {seed} peer {i}: {} presses", issued[i]);
                assert_eq!(
                    applied[i], received,
                    "seed {seed} peer {i}: presses applied vs received ({} issued)",
                    issued[i]
                );
                assert_eq!(
                    received, issued[i],
                    "seed {seed} peer {i}: presses received vs issued"
                );
            }
            assert!(
                d.late_merged(PeerId(4)) > 0,
                "seed {seed}: the far peer's inputs came late"
            );
        }
    }

    #[test]
    fn clock_steps_whole_ticks_and_caps_catch_up() {
        let mut c = TickClock::new(30, 4);
        assert_eq!(c.advance(20), 0);
        assert_eq!(c.advance(20), 1, "40 ms is one 33.3 ms tick");
        assert_eq!(c.advance(1000), 4, "catch-up capped");
        assert_eq!(c.advance(0), 0, "the rest of the stall was dropped");
        let mut wild = TickClock::new(u32::MAX, 1);
        assert_eq!(
            wild.tick_rate_hz(),
            1000,
            "absurd rates are clamped, never a zero-length tick"
        );
        assert_eq!(wild.advance(5), 1);
    }

    // ---- spec 120 § 6.1 (ADR-8312) ----

    fn ticks(h: &InputHistory<u32>, max_bytes: usize) -> Vec<Vec<u64>> {
        h.bundles_within(max_bytes, 8)
            .iter()
            .map(|b| b.iter().map(|(t, _)| t).collect())
            .collect()
    }

    #[test]
    fn a_gap_keeps_the_older_run_as_a_second_bundle() {
        let mut h = InputHistory::new(8);
        for t in 10..15u64 {
            h.push(t, t as u32);
        }
        h.push(17, 17); // 15 and 16 skipped (a clock step): the run 10..=14 is still useful
        assert_eq!(h.len(), 6);
        assert_eq!(ticks(&h, 1200), vec![vec![17], vec![10, 11, 12, 13, 14]]);
        assert_eq!(h.bundle().map(|b| (b.first_tick, b.inputs)), Some((17, vec![17])));
    }

    #[test]
    fn a_repeated_tick_keeps_its_first_input_and_an_older_tick_is_inserted() {
        let mut h = InputHistory::new(8);
        h.push(5, 50u32);
        h.push(6, 60);
        h.push(6, 61); // a second frame inside tick 6
        h.push(4, 40); // a late sample for an older tick
        assert_eq!(h.len(), 3);
        let b = h.bundle().expect("bundle");
        assert_eq!((b.first_tick, b.inputs), (4, vec![40, 50, 60]));
    }

    #[test]
    fn acknowledged_inputs_are_never_sent_again() {
        let mut h = InputHistory::new(MAX_BUNDLE_INPUTS);
        for t in 0..8u64 {
            h.push(t, t as u32);
        }
        h.ack_through(5);
        assert_eq!(ticks(&h, 1200), vec![vec![6, 7]]);
        h.ack_through(3); // reordered older feedback: no effect
        assert_eq!(h.acked_through(), Some(5));
        // A long burst: the 32 newest unacknowledged inputs are kept and all ride in one bundle.
        for t in 8..48u64 {
            h.push(t, t as u32);
        }
        assert_eq!(h.len(), MAX_BUNDLE_INPUTS);
        assert_eq!(ticks(&h, 1200), vec![(16..48).collect::<Vec<_>>()]);
        h.push(4, 4); // already acknowledged
        assert_eq!(h.len(), MAX_BUNDLE_INPUTS);
        h.ack_through(40);
        assert_eq!(ticks(&h, 1200), vec![(41..48).collect::<Vec<_>>()]);
    }

    /// A configured redundancy below the loss-aware copy count is raised to it.
    #[test]
    fn the_loss_aware_count_raises_a_low_redundancy() {
        let mut h = InputHistory::new(1);
        for t in 0..5u64 {
            h.push(t, t as u32);
        }
        assert_eq!(h.len(), 1);
        h.set_copies_floor(loss_aware_copies(0.05));
        for t in 5..10u64 {
            h.push(t, t as u32);
        }
        assert_eq!(ticks(&h, 1200), vec![vec![7, 8, 9]], "5 % loss: 3 copies");
        h.set_copies_floor(loss_aware_copies(0.0));
        h.push(10, 10);
        assert_eq!(h.len(), 1, "a clean link keeps the configured 1");
    }

    #[test]
    fn loss_aware_copies_reach_a_residual_loss_of_a_thousandth() {
        assert_eq!(loss_aware_copies(0.0), 1);
        assert_eq!(loss_aware_copies(0.01), 2);
        assert_eq!(loss_aware_copies(0.05), 3);
        assert_eq!(loss_aware_copies(0.1), 3);
        assert_eq!(loss_aware_copies(0.2), 5);
        assert_eq!(loss_aware_copies(0.5), 10);
        assert_eq!(loss_aware_copies(f64::NAN), 10);
        for loss in [0.01, 0.03, 0.05, 0.1, 0.2, 0.3] {
            let n = i32::from(loss_aware_copies(loss));
            assert!(loss.powi(n) <= RESIDUAL_INPUT_LOSS + 1e-12, "{loss}: {n}");
            assert!(
                loss.powi(n - 1) > RESIDUAL_INPUT_LOSS,
                "{loss}: {n} is the smallest"
            );
        }
    }

    /// Inputs that need a whole bundle each: older bundles are sent only until each input rode
    /// in `min_copies` datagrams.
    #[test]
    fn older_bundles_stop_once_their_inputs_have_enough_copies() {
        let mut h: InputHistory<Vec<u8>> = InputHistory::new(8);
        let budget = BUNDLE_HEADER_BYTES + 502; // one 500-byte input (+2 length bytes) per bundle
        let mut per_send = Vec::new();
        for t in 0..6u64 {
            h.push(t, vec![t as u8; 500]);
            let sent = h.bundles_for_send(budget, MAX_INPUT_BUNDLES_PER_SEND, 3);
            per_send.push(sent.iter().map(|b| b.first_tick).collect::<Vec<_>>());
        }
        // Each input rides in exactly 3 datagrams: as the newest, then twice as an older bundle.
        assert_eq!(per_send[0], vec![0]);
        assert_eq!(per_send[1], vec![1, 0]);
        assert_eq!(per_send[2], vec![2, 1, 0]);
        assert_eq!(per_send[3], vec![3, 2, 1]);
        assert_eq!(per_send[5], vec![5, 4, 3]);
    }

    /// A codec with its own delta: a repeated input costs one byte.
    #[derive(Debug, Clone, PartialEq)]
    struct DeltaPad(u64);
    impl NetCodec for DeltaPad {
        fn encode(&self, w: &mut WireWriter) {
            w.u64(self.0);
        }
        fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
            Ok(Self(r.u64()?))
        }
        fn encode_delta(&self, previous: &Self, w: &mut WireWriter) {
            if self == previous {
                w.u8(0);
            } else {
                w.u8(1);
                self.encode(w);
            }
        }
        fn decode_delta(previous: &Self, r: &mut WireReader<'_>) -> Result<Self, WireError> {
            match r.u8()? {
                0 => Ok(previous.clone()),
                _ => Self::decode(r),
            }
        }
    }

    #[test]
    fn bundles_code_each_input_against_the_one_before() {
        let bundle = InputBundle {
            first_tick: 7,
            inputs: vec![DeltaPad(1), DeltaPad(1), DeltaPad(1), DeltaPad(2)],
        };
        let mut w = WireWriter::new();
        bundle.encode(&mut w);
        let bytes = w.finish();
        assert_eq!(bytes.len(), BUNDLE_HEADER_BYTES + 8 + 1 + 1 + 9);
        let back = InputBundle::<DeltaPad>::decode(&mut WireReader::new(&bytes)).expect("valid");
        assert_eq!(back, bundle);
        // The default delta is the full value: a plain codec's bundle bytes are unchanged.
        let plain = InputBundle {
            first_tick: 7,
            inputs: vec![1u32, 1, 2],
        };
        let mut w = WireWriter::new();
        plain.encode(&mut w);
        let mut expected = WireWriter::new();
        expected.u64(7);
        expected.u8(3);
        for v in [1u32, 1, 2] {
            v.encode(&mut expected);
        }
        assert_eq!(w.finish(), expected.finish());
    }
}
