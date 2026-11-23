// SPDX-License-Identifier: MPL-2.0
//! Lockstep payload codecs (spec 120 § 7.2).
//!
//! Every decoder here reads untrusted bytes: lengths are checked against what the buffer holds
//! and against fixed caps before anything is allocated, and nothing panics.
//!
//! **Frame body** (bit-packed, zero-padded to a byte). The delta base is the state after the
//! previous tick ([`FrameState`]), so a body can only be applied in tick order:
//!
//! ```text
//! 1 bit  delay change     -> varint delay, varint (delay_from - tick - 1)
//! 1 bit  membership       -> varint count, then per change: varint seat, 1 bit leave
//!                            (seat 65536 with the leave bit is the match end, spec 120 § 7.4)
//! 1 bit  gap              (no seat has an input this tick; no entries follow)
//! per member seat, ascending:
//!   0      same as the seat's previous entry
//!   10     absent
//!   110    present, same length as the seat's last input: per byte 1 bit changed (+ 8 bits)
//!   111    present, in full: varint length, bytes
//! ```
//!
//! **Frames datagram** (host to seat): `kind u8 | seq | next_build | vote_ack+1 | base_tick |
//! count | flags u8 | [LinkFeedback] | (body length, body)*`, integers as varints. Bodies carry a
//! byte length so a receiver can step over frames it already has.
//!
//! **Inputs datagram** (seat to host): `kind u8 | seq | ack_next | down_p99_run | window run | lead | flags u8 |
//! votes u8 | (tick, hash u64)* | first_tick | count u8 | bits`, where the bits hold per input `0` (same as
//! the previous one in this datagram), `10` (none: a gap tick) or `11` + varint length + bytes.
//! Flags: bit 0 a hole, bit 1 the seat asks to leave, bit 2 the seat is still catching up.
//!
//! **Catch-up** (host to seat, stream; spec 120 § 7.4): `KIND_CATCHUP | state_tick | delay |
//! delay_from | seat count | (seat, flags u8, varint length, bytes)* | state length` (the delta base
//! of the frame chain after `state_tick`), then the game's state in `KIND_STATE_CHUNK | bytes`
//! payloads, then the frames after `state_tick` as stream frames.

use std::collections::BTreeMap;

use super::{LockstepError, MembershipChange, MembershipKind, SeatId};
use crate::bits::{BitReader, BitWriter};
use crate::desync::{self, MAX_SUBSYSTEM_NAME_BYTES, MAX_SUBSYSTEMS, ProbeRows, SubsystemRow};
use crate::feedback::{LINK_FEEDBACK_BYTES, LinkFeedback};
use crate::wire::{WireReader, WireWriter};

/// Frames datagram, host to seat.
pub(crate) const KIND_FRAMES: u8 = 0x01;
/// Inputs datagram, seat to host.
pub(crate) const KIND_INPUTS: u8 = 0x02;
/// Stream, host to seat: frames that did not fit a datagram.
pub(crate) const KIND_STREAM_FRAMES: u8 = 0x10;
/// Stream, host to seat: a quorum order.
pub(crate) const KIND_ORDER: u8 = 0x11;
/// Stream, host to seat: a catch-up begins (the delta base after the state's tick).
pub(crate) const KIND_CATCHUP: u8 = 0x12;
/// Stream, host to seat: a piece of the game's state for a catch-up.
pub(crate) const KIND_STATE_CHUNK: u8 = 0x13;
/// Stream, seat to host: a vote after a self-check, or the news that a state transfer was applied.
pub(crate) const KIND_REVOTE: u8 = 0x20;
/// Stream, seat to host: per-tick hashes for a probe.
pub(crate) const KIND_PROBE_ROWS: u8 = 0x21;
/// Stream, seat to host: subsystem rows for the probe's second stage (spec 120 § 7.6, ADR-8211).
pub(crate) const KIND_SUBSYSTEM_ROWS: u8 = 0x22;

/// Frames one payload may carry.
pub(crate) const MAX_FRAMES_PER_PAYLOAD: usize = 256;
/// Inputs one inputs datagram may carry.
pub(crate) const MAX_INPUTS_PER_DATAGRAM: usize = 32;
/// Checkpoint votes one inputs datagram may carry.
pub(crate) const MAX_VOTES_PER_DATAGRAM: usize = 4;
/// Membership changes one frame may carry.
pub(crate) const MAX_MEMBERSHIP_PER_FRAME: usize = 1024;
/// Seats one frame may list.
pub(crate) const MAX_SEATS: usize = 4096;
/// Per-tick hashes one probe reply may carry.
pub(crate) const MAX_PROBE_ROWS: usize = 1024;
/// Subsystem rows one subsystem-probe reply may carry (the host asks for two).
pub(crate) const MAX_SUBSYSTEM_ROWS: usize = 16;
/// Longest input the byte-mask delta is used for.
const MASK_MAX_BYTES: usize = 64;
/// The membership entry that marks the match end (one past the largest seat id).
const END_MARKER: u64 = 1 << 16;
/// Largest game state a catch-up carries, bytes.
pub(crate) const MAX_STATE_BYTES: usize = 256 << 20;
/// Inputs flags.
pub(crate) const FLAG_HOLE: u8 = 1;
pub(crate) const FLAG_LEAVE: u8 = 2;
pub(crate) const FLAG_CATCHING_UP: u8 = 4;

/// A byte cursor with varints.
struct Cur<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, LockstepError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or(LockstepError::Malformed("truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, LockstepError> {
        let mut out = 0u64;
        for i in 0..10u32 {
            let b = u64::from(self.u8()?);
            let shift = i * 7;
            if shift == 63 && (b & 0x7e) != 0 {
                return Err(LockstepError::Malformed("varint overflows 64 bits"));
            }
            out |= (b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(LockstepError::Malformed("varint longer than 10 bytes"))
    }

    fn len(&mut self, max: usize, what: &'static str) -> Result<usize, LockstepError> {
        let n = usize::try_from(self.varint()?).unwrap_or(usize::MAX);
        if n > max {
            return Err(LockstepError::Malformed(what));
        }
        Ok(n)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], LockstepError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(LockstepError::Malformed("truncated"))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(LockstepError::Malformed("truncated"))?;
        self.pos = end;
        Ok(s)
    }

    fn u64le(&mut self) -> Result<u64, LockstepError> {
        let s = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = self.buf.get(self.pos..).unwrap_or_default();
        self.pos = self.buf.len();
        s
    }

    fn finish(&self) -> Result<(), LockstepError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(LockstepError::Malformed("trailing bytes"))
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let group = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}

/// One seat's delta base: its previous entry and the last input bytes it had.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SeatDelta {
    /// The previous non-gap tick listed the seat present.
    pub(crate) present: bool,
    /// The seat's last input bytes (kept across absent ticks).
    pub(crate) bytes: Vec<u8>,
    /// The seat has had an input since it joined: `bytes` is a real input.
    pub(crate) seen: bool,
}

/// The delta base of the frame chain: the state after some tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameState {
    /// Member seats and their delta bases, ascending.
    pub(crate) seats: BTreeMap<SeatId, SeatDelta>,
    /// The newest delay epoch.
    pub(crate) delay: u16,
    /// Its first capture tick.
    pub(crate) delay_from: u64,
}

impl FrameState {
    pub(crate) fn new(initial_delay: u16) -> Self {
        Self {
            seats: BTreeMap::new(),
            delay: initial_delay,
            delay_from: 0,
        }
    }
}

/// One tick as the host decided it, before encoding: what changed and each member's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawFrame {
    pub(crate) tick: u64,
    /// `Some((delay, delay_from))` when this frame starts a new delay epoch.
    pub(crate) delay_change: Option<(u16, u64)>,
    pub(crate) membership: Vec<MembershipChange>,
    /// No capture maps to this tick: every member is absent and no entries are written.
    pub(crate) gap: bool,
    /// Each member's input bytes this tick (`None`: absent), ascending by seat; empty for a gap.
    pub(crate) entries: Vec<(SeatId, Option<Vec<u8>>)>,
    /// The match ends with this frame.
    pub(crate) match_end: bool,
}

/// Encodes `frame` against `state` and moves `state` to the state after it.
pub(crate) fn encode_frame_body(frame: &RawFrame, state: &mut FrameState) -> Vec<u8> {
    let mut w = BitWriter::with_capacity(16);
    match frame.delay_change {
        Some((delay, from)) => {
            w.write_bool(true);
            w.write_varint(u64::from(delay));
            w.write_varint(from.saturating_sub(frame.tick + 1));
            state.delay = delay;
            state.delay_from = from;
        }
        None => w.write_bool(false),
    }
    w.write_bool(!frame.membership.is_empty() || frame.match_end);
    if !frame.membership.is_empty() || frame.match_end {
        w.write_varint(frame.membership.len() as u64 + u64::from(frame.match_end));
        if frame.match_end {
            w.write_varint(END_MARKER);
            w.write_bool(true);
        }
        for change in &frame.membership {
            w.write_varint(u64::from(change.seat.0));
            w.write_bool(change.kind == MembershipKind::Leave);
            match change.kind {
                MembershipKind::Join => {
                    state.seats.entry(change.seat).or_default();
                }
                MembershipKind::Leave => {
                    state.seats.remove(&change.seat);
                }
            }
        }
    }
    w.write_bool(frame.gap);
    if frame.gap {
        return w.finish();
    }
    for (seat, entry) in &frame.entries {
        let Some(base) = state.seats.get_mut(seat) else {
            continue;
        };
        match entry {
            None if !base.present => w.write_bool(false),
            None => {
                w.write_bits(0b01, 2); // 1, 0
                base.present = false;
            }
            Some(bytes) if base.present && *bytes == base.bytes => w.write_bool(false),
            Some(bytes) => {
                let changed = bytes.iter().zip(&base.bytes).filter(|(a, b)| a != b).count();
                let mask_bits = bytes.len() + 8 * changed;
                let full_bits = crate::bits::varint_bits(bytes.len() as u64) + 8 * bytes.len();
                let same_len = bytes.len() == base.bytes.len() && !bytes.is_empty();
                if same_len && bytes.len() <= MASK_MAX_BYTES && mask_bits < full_bits {
                    w.write_bits(0b011, 3); // 1, 1, 0
                    for (new, old) in bytes.iter().zip(&base.bytes) {
                        w.write_bool(new != old);
                        if new != old {
                            w.write_bits(u64::from(*new), 8);
                        }
                    }
                } else {
                    w.write_bits(0b111, 3);
                    w.write_varint(bytes.len() as u64);
                    w.write_bytes(bytes);
                }
                base.present = true;
                base.seen = true;
                base.bytes.clone_from(bytes);
            }
        }
    }
    w.finish()
}

/// Decodes a frame body for `tick` against `state` (the state after `tick - 1`) and moves `state`
/// on. `max_input` bounds each input's length. On an error `state` must be discarded.
pub(crate) fn decode_frame_body(
    body: &[u8],
    tick: u64,
    state: &mut FrameState,
    max_input: usize,
) -> Result<RawFrame, LockstepError> {
    let mut r = BitReader::new(body);
    let mut delay_change = None;
    if r.read_bool()? {
        let delay = u16::try_from(r.read_varint()?).map_err(|_| LockstepError::Malformed("delay"))?;
        let from = (tick + 1)
            .checked_add(r.read_varint()?)
            .ok_or(LockstepError::Malformed("delay_from"))?;
        if from < state.delay_from {
            return Err(LockstepError::Malformed("delay epoch moves backward"));
        }
        state.delay = delay;
        state.delay_from = from;
        delay_change = Some((delay, from));
    }
    let mut membership = Vec::new();
    let mut match_end = false;
    if r.read_bool()? {
        let n = r.read_len(MAX_MEMBERSHIP_PER_FRAME)?;
        // Each change takes at least 9 bits: a short body cannot claim a long list.
        if n * 9 > r.remaining_bits() {
            return Err(LockstepError::Malformed("membership count past the body"));
        }
        membership.reserve(n);
        for _ in 0..n {
            let raw_seat = r.read_varint()?;
            if raw_seat == END_MARKER {
                if !r.read_bool()? || match_end {
                    return Err(LockstepError::Malformed("match end marker"));
                }
                match_end = true;
                continue;
            }
            let seat = SeatId(u16::try_from(raw_seat).map_err(|_| LockstepError::Malformed("seat id"))?);
            let kind = if r.read_bool()? {
                MembershipKind::Leave
            } else {
                MembershipKind::Join
            };
            match kind {
                MembershipKind::Join => {
                    if state.seats.len() >= MAX_SEATS && !state.seats.contains_key(&seat) {
                        return Err(LockstepError::Malformed("too many seats"));
                    }
                    state.seats.entry(seat).or_default();
                }
                MembershipKind::Leave => {
                    state.seats.remove(&seat);
                }
            }
            membership.push(MembershipChange { seat, kind });
        }
    }
    let gap = r.read_bool()?;
    let mut entries = Vec::new();
    if !gap {
        entries.reserve(state.seats.len());
        for (seat, base) in &mut state.seats {
            if !r.read_bool()? {
                entries.push((*seat, base.present.then(|| base.bytes.clone())));
                continue;
            }
            if !r.read_bool()? {
                base.present = false;
                entries.push((*seat, None));
                continue;
            }
            if r.read_bool()? {
                let n = r.read_len(max_input)?;
                base.bytes = r.read_bytes(n)?;
            } else {
                if !base.seen {
                    return Err(LockstepError::Malformed("byte mask without a base"));
                }
                if base.bytes.is_empty() || base.bytes.len() > MASK_MAX_BYTES {
                    return Err(LockstepError::Malformed("byte mask without a base"));
                }
                for b in &mut base.bytes {
                    if r.read_bool()? {
                        *b = r.read_bits(8)? as u8;
                    }
                }
            }
            base.present = true;
            base.seen = true;
            entries.push((*seat, Some(base.bytes.clone())));
        }
    }
    // What is left must be the zero padding of the last byte.
    if r.remaining_bits() >= 8 || r.read_bits(r.remaining_bits() as u32)? != 0 {
        return Err(LockstepError::Malformed("trailing bits after the frame"));
    }
    Ok(RawFrame {
        tick,
        delay_change,
        membership,
        gap,
        entries,
        match_end,
    })
}

/// The header of a frames datagram.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FramesHeader {
    /// The host's datagram sequence number toward this seat.
    pub(crate) seq: u64,
    /// The host's next tick to build: inputs for earlier ticks are spent.
    pub(crate) next_build: u64,
    /// The newest checkpoint tick the host holds this seat's vote for.
    pub(crate) vote_ack: Option<u64>,
    /// Tick of the first body.
    pub(crate) base_tick: u64,
    /// The seat's input timing as the host measured it, when this datagram carries a record.
    pub(crate) feedback: Option<LinkFeedback>,
}

fn put_bodies<'a>(out: &mut Vec<u8>, bodies: impl Iterator<Item = &'a [u8]>) {
    for body in bodies {
        put_varint(out, body.len() as u64);
        out.extend_from_slice(body);
    }
}

fn get_bodies<'a>(c: &mut Cur<'a>, count: usize) -> Result<Vec<&'a [u8]>, LockstepError> {
    // Every body takes at least its length byte.
    if count > c.buf.len().saturating_sub(c.pos) {
        return Err(LockstepError::Malformed("frame count past the payload"));
    }
    let mut bodies = Vec::with_capacity(count);
    for _ in 0..count {
        let n = c.len(c.buf.len(), "frame body length")?;
        bodies.push(c.bytes(n)?);
    }
    Ok(bodies)
}

fn varint_len(v: u64) -> usize {
    crate::bits::varint_bits(v) / 8
}

/// Bytes a frames datagram with this header takes around its bodies (the count takes at most two).
pub(crate) fn frames_header_len(header: &FramesHeader) -> usize {
    1 + varint_len(header.seq)
        + varint_len(header.next_build)
        + varint_len(header.vote_ack.map_or(0, |t| t.saturating_add(1)))
        + varint_len(header.base_tick)
        + 2
        + 1
        + if header.feedback.is_some() {
            LINK_FEEDBACK_BYTES
        } else {
            0
        }
}

/// Bytes one body takes inside a payload.
pub(crate) fn body_cost(body: &[u8]) -> usize {
    varint_len(body.len() as u64) + body.len()
}

pub(crate) fn encode_frames_datagram<'a>(
    header: &FramesHeader,
    count: usize,
    bodies: impl Iterator<Item = &'a [u8]>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(KIND_FRAMES);
    put_varint(&mut out, header.seq);
    put_varint(&mut out, header.next_build);
    put_varint(&mut out, header.vote_ack.map_or(0, |t| t.saturating_add(1)));
    put_varint(&mut out, header.base_tick);
    put_varint(&mut out, count as u64);
    match &header.feedback {
        Some(fb) => {
            out.push(1);
            let mut w = WireWriter::with_capacity(LINK_FEEDBACK_BYTES);
            fb.encode(&mut w);
            out.extend_from_slice(w.as_slice());
        }
        None => out.push(0),
    }
    put_bodies(&mut out, bodies);
    out
}

pub(crate) fn decode_frames_datagram(bytes: &[u8]) -> Result<(FramesHeader, Vec<&[u8]>), LockstepError> {
    let mut c = Cur::new(bytes);
    if c.u8()? != KIND_FRAMES {
        return Err(LockstepError::Malformed("not a frames datagram"));
    }
    let seq = c.varint()?;
    let next_build = c.varint()?;
    let vote_ack = c.varint()?.checked_sub(1);
    let base_tick = c.varint()?;
    let count = c.len(MAX_FRAMES_PER_PAYLOAD, "too many frames")?;
    let feedback = match c.u8()? {
        0 => None,
        1 => {
            let raw = c.bytes(LINK_FEEDBACK_BYTES)?;
            let mut r = WireReader::new(raw);
            Some(LinkFeedback::decode(&mut r)?)
        }
        _ => return Err(LockstepError::Malformed("frames flags")),
    };
    let bodies = get_bodies(&mut c, count)?;
    c.finish()?;
    if base_tick.checked_add(count as u64).is_none() {
        return Err(LockstepError::Malformed("frame ticks overflow"));
    }
    Ok((
        FramesHeader {
            seq,
            next_build,
            vote_ack,
            base_tick,
            feedback,
        },
        bodies,
    ))
}

pub(crate) fn encode_stream_frames<'a>(
    base_tick: u64,
    count: usize,
    bodies: impl Iterator<Item = &'a [u8]>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(KIND_STREAM_FRAMES);
    put_varint(&mut out, base_tick);
    put_varint(&mut out, count as u64);
    put_bodies(&mut out, bodies);
    out
}

pub(crate) fn decode_stream_frames(bytes: &[u8]) -> Result<(u64, Vec<&[u8]>), LockstepError> {
    let mut c = Cur::new(bytes);
    if c.u8()? != KIND_STREAM_FRAMES {
        return Err(LockstepError::Malformed("not stream frames"));
    }
    let base_tick = c.varint()?;
    let count = c.len(MAX_FRAMES_PER_PAYLOAD, "too many frames")?;
    let bodies = get_bodies(&mut c, count)?;
    c.finish()?;
    if base_tick.checked_add(count as u64).is_none() {
        return Err(LockstepError::Malformed("frame ticks overflow"));
    }
    Ok((base_tick, bodies))
}

/// A seat's inputs datagram.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InputsMsg {
    /// The seat's datagram sequence number.
    pub(crate) seq: u64,
    /// The next frame tick the seat needs: every earlier frame arrived.
    pub(crate) ack_next: u64,
    /// The p99 loss run the seat measured on the frames datagrams it receives.
    pub(crate) down_p99_run: u32,
    /// The loss run the seat's redundancy window should cover (the fit without the cap).
    pub(crate) down_window_run: u32,
    /// Ticks the seat stamps ahead of the frame delay.
    pub(crate) lead: u16,
    /// The seat holds a frames datagram that starts past the frames it has: frames are missing
    /// that no datagram in flight repeats. The host answers with every unacknowledged frame.
    pub(crate) hole: bool,
    /// The seat asks to leave the match (agreed on the next frame built).
    pub(crate) leave: bool,
    /// The seat is still stepping the frames of a catch-up: it joins (or resumes) once clear.
    pub(crate) catching_up: bool,
    /// `(checkpoint tick, state hash)` votes the host has not acknowledged.
    pub(crate) votes: Vec<(u64, u64)>,
    /// Tick of the first input.
    pub(crate) first_tick: u64,
    /// Inputs for `first_tick`, `first_tick + 1`, ...; `None` for a tick the seat has no input for.
    pub(crate) inputs: Vec<Option<Vec<u8>>>,
}

pub(crate) fn encode_inputs(msg: &InputsMsg) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    out.push(KIND_INPUTS);
    put_varint(&mut out, msg.seq);
    put_varint(&mut out, msg.ack_next);
    put_varint(&mut out, u64::from(msg.down_p99_run));
    put_varint(&mut out, u64::from(msg.down_window_run));
    put_varint(&mut out, u64::from(msg.lead));
    out.push(
        (u8::from(msg.hole) * FLAG_HOLE)
            | (u8::from(msg.leave) * FLAG_LEAVE)
            | (u8::from(msg.catching_up) * FLAG_CATCHING_UP),
    );
    let votes = msg.votes.len().min(MAX_VOTES_PER_DATAGRAM);
    out.push(votes as u8);
    for (tick, hash) in msg.votes.iter().take(votes) {
        put_varint(&mut out, *tick);
        out.extend_from_slice(&hash.to_le_bytes());
    }
    put_varint(&mut out, msg.first_tick);
    let count = msg.inputs.len().min(MAX_INPUTS_PER_DATAGRAM);
    out.push(count as u8);
    let mut w = BitWriter::with_capacity(16);
    let mut prev: Option<&Vec<u8>> = None;
    for input in msg.inputs.iter().take(count) {
        match input {
            None => w.write_bits(0b01, 2),
            Some(bytes) if prev == Some(bytes) => w.write_bool(false),
            Some(bytes) => {
                w.write_bits(0b11, 2);
                w.write_varint(bytes.len() as u64);
                w.write_bytes(bytes);
                prev = Some(bytes);
            }
        }
    }
    out.extend_from_slice(w.as_bytes());
    out
}

pub(crate) fn decode_inputs(bytes: &[u8], max_input: usize) -> Result<InputsMsg, LockstepError> {
    let mut c = Cur::new(bytes);
    if c.u8()? != KIND_INPUTS {
        return Err(LockstepError::Malformed("not an inputs datagram"));
    }
    let seq = c.varint()?;
    let ack_next = c.varint()?;
    let down_p99_run = u32::try_from(c.varint()?).map_err(|_| LockstepError::Malformed("loss run"))?;
    let down_window_run = u32::try_from(c.varint()?).map_err(|_| LockstepError::Malformed("loss run"))?;
    let lead = u16::try_from(c.varint()?).map_err(|_| LockstepError::Malformed("lead"))?;
    let flags = c.u8()?;
    if flags & !(FLAG_HOLE | FLAG_LEAVE | FLAG_CATCHING_UP) != 0 {
        return Err(LockstepError::Malformed("inputs flags"));
    }
    let hole = flags & FLAG_HOLE != 0;
    let leave = flags & FLAG_LEAVE != 0;
    let catching_up = flags & FLAG_CATCHING_UP != 0;
    let n_votes = usize::from(c.u8()?);
    if n_votes > MAX_VOTES_PER_DATAGRAM {
        return Err(LockstepError::Malformed("too many votes"));
    }
    let mut votes = Vec::with_capacity(n_votes);
    for _ in 0..n_votes {
        let tick = c.varint()?;
        votes.push((tick, c.u64le()?));
    }
    let first_tick = c.varint()?;
    let count = usize::from(c.u8()?);
    if count > MAX_INPUTS_PER_DATAGRAM {
        return Err(LockstepError::Malformed("too many inputs"));
    }
    if first_tick.checked_add(count as u64).is_none() {
        return Err(LockstepError::Malformed("input ticks overflow"));
    }
    let mut r = BitReader::new(c.rest());
    let mut inputs: Vec<Option<Vec<u8>>> = Vec::with_capacity(count);
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..count {
        if !r.read_bool()? {
            let Some(p) = &prev else {
                return Err(LockstepError::Malformed("repeat without an input before it"));
            };
            inputs.push(Some(p.clone()));
        } else if !r.read_bool()? {
            inputs.push(None);
        } else {
            let n = r.read_len(max_input).map_err(|e| match e {
                crate::bits::DecodeError::TooLong { len, max } => LockstepError::InputTooLarge { len, max },
                other => LockstepError::Bits(other),
            })?;
            let b = r.read_bytes(n)?;
            prev = Some(b.clone());
            inputs.push(Some(b));
        }
    }
    if r.remaining_bits() >= 8 || r.read_bits(r.remaining_bits() as u32)? != 0 {
        return Err(LockstepError::Malformed("trailing bits after the inputs"));
    }
    Ok(InputsMsg {
        seq,
        ack_next,
        down_p99_run,
        down_window_run,
        lead,
        hole,
        leave,
        catching_up,
        votes,
        first_tick,
        inputs,
    })
}

/// A quorum order from the host (stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// Replay from `from` (the last agreed checkpoint; `None`: the start) and vote again on `tick`.
    Recheck { tick: u64, from: Option<u64> },
    /// Your state differs from the majority's at `tick`: take the state of `source` at `tick`.
    Resync { tick: u64, source: WireVoter },
    /// Send your state at `tick` to `target`.
    ProvideState { tick: u64, target: WireVoter },
    /// Send your per-tick hashes for `from..=to`.
    Probe { from: u64, to: u64 },
    /// Send your subsystem rows for `from..=to` (the probe's second stage).
    ProbeSubsystems { from: u64, to: u64 },
    /// The seat is removed from the match for repeated divergence.
    Removed { tick: u64 },
    /// The tally at `tick` cannot be decided: the match ends without blame.
    Unresolved { tick: u64 },
}

/// A voter on the wire: `None` is the host's observer simulation.
pub(crate) type WireVoter = Option<SeatId>;

fn put_voter(out: &mut Vec<u8>, voter: WireVoter) {
    put_varint(out, voter.map_or(0, |s| u64::from(s.0) + 1));
}

fn get_voter(c: &mut Cur<'_>) -> Result<WireVoter, LockstepError> {
    match c.varint()? {
        0 => Ok(None),
        n => Ok(Some(SeatId(
            u16::try_from(n - 1).map_err(|_| LockstepError::Malformed("seat id"))?,
        ))),
    }
}

pub(crate) fn encode_order(order: &Order) -> Vec<u8> {
    let mut out = vec![KIND_ORDER];
    match *order {
        Order::Recheck { tick, from } => {
            out.push(0);
            put_varint(&mut out, tick);
            put_varint(&mut out, from.map_or(0, |t| t + 1));
        }
        Order::Resync { tick, source } => {
            out.push(1);
            put_varint(&mut out, tick);
            put_voter(&mut out, source);
        }
        Order::ProvideState { tick, target } => {
            out.push(2);
            put_varint(&mut out, tick);
            put_voter(&mut out, target);
        }
        Order::Probe { from, to } => {
            out.push(3);
            put_varint(&mut out, from);
            put_varint(&mut out, to);
        }
        Order::Removed { tick } => {
            out.push(4);
            put_varint(&mut out, tick);
        }
        Order::Unresolved { tick } => {
            out.push(5);
            put_varint(&mut out, tick);
        }
        Order::ProbeSubsystems { from, to } => {
            out.push(6);
            put_varint(&mut out, from);
            put_varint(&mut out, to);
        }
    }
    out
}

pub(crate) fn decode_order(bytes: &[u8]) -> Result<Order, LockstepError> {
    let mut c = Cur::new(bytes);
    if c.u8()? != KIND_ORDER {
        return Err(LockstepError::Malformed("not an order"));
    }
    let order = match c.u8()? {
        0 => {
            let tick = c.varint()?;
            Order::Recheck {
                tick,
                from: c.varint()?.checked_sub(1),
            }
        }
        1 => {
            let tick = c.varint()?;
            Order::Resync {
                tick,
                source: get_voter(&mut c)?,
            }
        }
        2 => {
            let tick = c.varint()?;
            Order::ProvideState {
                tick,
                target: get_voter(&mut c)?,
            }
        }
        3 => {
            let from = c.varint()?;
            let to = c.varint()?;
            if to < from || to - from >= MAX_PROBE_ROWS as u64 {
                return Err(LockstepError::Malformed("probe range"));
            }
            Order::Probe { from, to }
        }
        4 => Order::Removed { tick: c.varint()? },
        5 => Order::Unresolved { tick: c.varint()? },
        6 => {
            let from = c.varint()?;
            let to = c.varint()?;
            if to < from || to - from >= MAX_SUBSYSTEM_ROWS as u64 {
                return Err(LockstepError::Malformed("subsystem probe range"));
            }
            Order::ProbeSubsystems { from, to }
        }
        _ => return Err(LockstepError::Malformed("order kind")),
    };
    c.finish()?;
    Ok(order)
}

/// A seat's stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SeatStream {
    /// The seat's hash at `tick` after a self-check (`round` 1) or a state transfer (`round` 2).
    Revote { tick: u64, round: u8, hash: u64 },
    /// Per-tick hashes for `from`, `from + 1`, ...
    ProbeRows { from: u64, hashes: Vec<u64> },
    /// Subsystem rows for the probe's second stage.
    SubsystemRows(ProbeRows),
}

pub(crate) fn encode_seat_stream(msg: &SeatStream) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    match msg {
        SeatStream::Revote { tick, round, hash } => {
            out.push(KIND_REVOTE);
            put_varint(&mut out, *tick);
            out.push(*round);
            out.extend_from_slice(&hash.to_le_bytes());
        }
        SeatStream::ProbeRows { from, hashes } => {
            out.push(KIND_PROBE_ROWS);
            put_varint(&mut out, *from);
            let n = hashes.len().min(MAX_PROBE_ROWS);
            put_varint(&mut out, n as u64);
            for h in hashes.iter().take(n) {
                out.extend_from_slice(&h.to_le_bytes());
            }
        }
        SeatStream::SubsystemRows(rows) => encode_subsystem_rows(&mut out, rows),
    }
    out
}

/// `kind | names (varint count, then varint length + UTF-8 each) | rows (varint count, then per
/// row varint tick, u64 world, one u64 per name)`. Names past [`MAX_SUBSYSTEMS`], long names and
/// rows past [`MAX_SUBSYSTEM_ROWS`] are cut, so what is sent always decodes.
fn encode_subsystem_rows(out: &mut Vec<u8>, rows: &ProbeRows) {
    out.push(KIND_SUBSYSTEM_ROWS);
    let names = rows.names.len().min(MAX_SUBSYSTEMS);
    put_varint(out, names as u64);
    for name in rows.names.iter().take(names) {
        let name = desync::clip_name(name);
        put_varint(out, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
    }
    let n = rows.rows.len().min(MAX_SUBSYSTEM_ROWS);
    put_varint(out, n as u64);
    for row in rows.rows.iter().take(n) {
        put_varint(out, row.tick);
        out.extend_from_slice(&row.world.to_le_bytes());
        for i in 0..names {
            out.extend_from_slice(&row.hashes.get(i).copied().unwrap_or(0).to_le_bytes());
        }
    }
}

fn decode_subsystem_rows(c: &mut Cur<'_>) -> Result<ProbeRows, LockstepError> {
    let names_len = c.len(MAX_SUBSYSTEMS, "too many subsystems")?;
    let mut names = Vec::with_capacity(names_len);
    for _ in 0..names_len {
        let n = c.len(MAX_SUBSYSTEM_NAME_BYTES, "subsystem name too long")?;
        let name =
            std::str::from_utf8(c.bytes(n)?).map_err(|_| LockstepError::Malformed("subsystem name"))?;
        if names.iter().any(|known: &String| known == name) {
            return Err(LockstepError::Malformed("subsystem name repeated"));
        }
        names.push(name.to_owned());
    }
    let rows_len = c.len(MAX_SUBSYSTEM_ROWS, "too many subsystem rows")?;
    let mut rows: Vec<SubsystemRow> = Vec::with_capacity(rows_len);
    for _ in 0..rows_len {
        let tick = c.varint()?;
        if rows.last().is_some_and(|prev| prev.tick >= tick) {
            return Err(LockstepError::Malformed("subsystem rows out of order"));
        }
        let world = c.u64le()?;
        let mut hashes = Vec::with_capacity(names_len);
        for _ in 0..names_len {
            hashes.push(c.u64le()?);
        }
        rows.push(SubsystemRow { tick, world, hashes });
    }
    Ok(ProbeRows { names, rows })
}

pub(crate) fn decode_seat_stream(bytes: &[u8]) -> Result<SeatStream, LockstepError> {
    let mut c = Cur::new(bytes);
    let msg = match c.u8()? {
        KIND_REVOTE => {
            let tick = c.varint()?;
            let round = c.u8()?;
            if !(1..=2).contains(&round) {
                return Err(LockstepError::Malformed("revote round"));
            }
            SeatStream::Revote {
                tick,
                round,
                hash: c.u64le()?,
            }
        }
        KIND_PROBE_ROWS => {
            let from = c.varint()?;
            let n = c.len(MAX_PROBE_ROWS, "too many probe rows")?;
            if n * 8 > c.buf.len().saturating_sub(c.pos) {
                return Err(LockstepError::Malformed("probe rows past the payload"));
            }
            let mut hashes = Vec::with_capacity(n);
            for _ in 0..n {
                hashes.push(c.u64le()?);
            }
            SeatStream::ProbeRows { from, hashes }
        }
        KIND_SUBSYSTEM_ROWS => SeatStream::SubsystemRows(decode_subsystem_rows(&mut c)?),
        _ => return Err(LockstepError::Malformed("not a seat stream message")),
    };
    c.finish()?;
    Ok(msg)
}

/// The head of a catch-up: the delta base of the frame chain after `state_tick` and the length of
/// the game's state that follows in chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatchupHead {
    pub(crate) state_tick: u64,
    pub(crate) base: FrameState,
    pub(crate) state_len: usize,
}

pub(crate) fn encode_catchup(head: &CatchupHead) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(KIND_CATCHUP);
    put_varint(&mut out, head.state_tick);
    put_varint(&mut out, u64::from(head.base.delay));
    put_varint(&mut out, head.base.delay_from);
    put_varint(&mut out, head.base.seats.len() as u64);
    for (seat, delta) in &head.base.seats {
        put_varint(&mut out, u64::from(seat.0));
        out.push(u8::from(delta.present) | (u8::from(delta.seen) << 1));
        put_varint(&mut out, delta.bytes.len() as u64);
        out.extend_from_slice(&delta.bytes);
    }
    put_varint(&mut out, head.state_len as u64);
    out
}

pub(crate) fn decode_catchup(bytes: &[u8], max_input: usize) -> Result<CatchupHead, LockstepError> {
    let mut c = Cur::new(bytes);
    if c.u8()? != KIND_CATCHUP {
        return Err(LockstepError::Malformed("not a catch-up"));
    }
    let state_tick = c.varint()?;
    let delay = u16::try_from(c.varint()?).map_err(|_| LockstepError::Malformed("delay"))?;
    let delay_from = c.varint()?;
    let n = c.len(MAX_SEATS, "too many seats")?;
    // Each seat takes at least three bytes.
    if n * 3 > c.buf.len().saturating_sub(c.pos) {
        return Err(LockstepError::Malformed("seat count past the payload"));
    }
    let mut base = FrameState {
        seats: BTreeMap::new(),
        delay,
        delay_from,
    };
    for _ in 0..n {
        let seat = SeatId(u16::try_from(c.varint()?).map_err(|_| LockstepError::Malformed("seat id"))?);
        let flags = c.u8()?;
        if flags > 3 {
            return Err(LockstepError::Malformed("seat delta flags"));
        }
        let len = c.len(max_input, "input past the limit")?;
        let delta = SeatDelta {
            present: flags & 1 != 0,
            seen: flags & 2 != 0,
            bytes: c.bytes(len)?.to_vec(),
        };
        if base.seats.insert(seat, delta).is_some() {
            return Err(LockstepError::Malformed("seat listed twice"));
        }
    }
    let state_len = c.len(MAX_STATE_BYTES, "state past the limit")?;
    c.finish()?;
    Ok(CatchupHead {
        state_tick,
        base,
        state_len,
    })
}

pub(crate) fn encode_state_chunk(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len() + 1);
    out.push(KIND_STATE_CHUNK);
    out.extend_from_slice(chunk);
    out
}

/// The kind byte of a payload, when it has one.
pub(crate) fn kind_of(bytes: &[u8]) -> Option<u8> {
    bytes.first().copied()
}
