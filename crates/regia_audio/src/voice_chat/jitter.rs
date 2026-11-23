// SPDX-License-Identifier: MPL-2.0
//! Adaptive jitter buffer (`docs/specs/59-voice-chat.md` § 2.9).
//!
//! A pure state machine over opaque payload bytes. Time is the caller's frame counter (one frame
//! is 20 ms), never a clock. Sequence numbers use serial-number arithmetic (RFC 3550 Appendix
//! A.1), the depth target follows the interarrival jitter estimate (RFC 3550 § 6.4.1).
//! Time scaling lives in the speaker (`timescale`), which reads this buffer's depth; the buffer itself
//! is the same in the deterministic mode of § 7.

use super::packet::{FRAME_TIMESTAMP_STEP, TIMESTAMP_RATE};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Milliseconds per voice frame.
pub const FRAME_MS: u32 = 20;
/// Consecutive concealed frames before the output fades to silence (100 ms).
pub const MAX_CONCEALED: u32 = 5;
/// Packets seen before the jitter estimate may move the target depth.
pub const JITTER_WARMUP: u64 = 16;
/// Slots a packet may be ahead of the playout cursor before the buffer resynchronizes.
pub const WINDOW_FRAMES: i16 = 64;

/// Signed distance `a - b` in serial-number arithmetic.
#[must_use]
pub fn seq_diff(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

/// Whether `a` is later than `b` (serial-number arithmetic).
#[must_use]
pub fn seq_newer(a: u16, b: u16) -> bool {
    seq_diff(a, b) > 0
}

/// Jitter buffer bounds in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitterSettings {
    /// Smallest target depth.
    pub min_ms: u32,
    /// Largest target depth.
    pub max_ms: u32,
    /// Target depth at the start.
    pub start_ms: u32,
}

impl Default for JitterSettings {
    fn default() -> Self {
        Self {
            min_ms: 40,
            max_ms: 200,
            start_ms: 60,
        }
    }
}

/// What happened to an inserted packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Stored.
    Accepted,
    /// The slot already held that sequence number.
    Duplicate,
    /// Older than the playout cursor.
    Late,
    /// Far ahead of the cursor: the buffer restarted at this packet.
    Resynced,
}

/// What the receive path must output for the next 20 ms frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playout {
    /// Still prefilling (or recovering from an underrun): output silence.
    Buffering,
    /// A received payload: decode it.
    Frame(Vec<u8>),
    /// The frame was lost; the next payload is buffered: recover with its in-band redundancy.
    Recover(Vec<u8>),
    /// The frame was lost and nothing follows yet: run the decoder's concealment.
    Conceal,
    /// More than [`MAX_CONCEALED`] frames in a row were missing: output silence.
    Silence,
    /// The talk spurt ended and the buffer is drained.
    Finished,
}

/// Counters of one buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterStats {
    /// Packets stored.
    pub received: u64,
    /// Packets dropped as older than the cursor.
    pub late: u64,
    /// Packets dropped as duplicates.
    pub duplicates: u64,
    /// Restarts after a packet far ahead of the cursor.
    pub resyncs: u64,
    /// Frames played from received payloads.
    pub played: u64,
    /// Frames recovered from the next packet's redundancy.
    pub recovered: u64,
    /// Holes filled by a spare frame copy (the sender's `redundancy`).
    pub spare_filled: u64,
    /// Frames concealed by the decoder.
    pub concealed: u64,
    /// Frames output as silence after too many missing frames.
    pub silenced: u64,
    /// Times the buffer ran dry while playing.
    pub underruns: u64,
    /// Interarrival jitter estimate in milliseconds.
    pub jitter_ms: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Idle,
    Buffering,
    Playing,
}

/// The adaptive jitter buffer of one speaker.
#[derive(Clone, Debug)]
pub struct JitterBuffer {
    settings: JitterSettings,
    target_frames: u32,
    mode: Mode,
    resync: bool,
    next_seq: u16,
    slots: VecDeque<Option<Vec<u8>>>,
    end_seq: Option<u16>,
    missing_run: u32,
    ran_dry: bool,
    stats: JitterStats,
    jitter_units: f64,
    prev: Option<(u64, u32)>,
    /// The last [`pull`](Self::pull) played a talk spurt's end frame (read once by the speaker
    /// to reset its decoder, see `take_spurt_end`).
    spurt_ended: bool,
    /// Payload buffers returned by [`Self::pull_into`], reused by the next store.
    spare: Vec<Vec<u8>>,
}

/// What [`JitterBuffer::pull_into`] produced. Frame and recover bytes are in the scratch buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullKind {
    /// Still prefilling.
    Buffering,
    /// A received payload was copied into the scratch buffer.
    Frame,
    /// A lost frame recovered from the next payload, copied into the scratch buffer.
    Recover,
    /// Concealment, no payload.
    Conceal,
    /// Silence after too many missing frames.
    Silence,
    /// The talk spurt ended and the buffer is drained.
    Finished,
}

fn ms_to_frames(ms: u32) -> u32 {
    ms.div_ceil(FRAME_MS).max(1)
}

impl JitterBuffer {
    /// A buffer with the given bounds.
    #[must_use]
    pub fn new(settings: JitterSettings) -> Self {
        let min = ms_to_frames(settings.min_ms);
        let max = ms_to_frames(settings.max_ms).max(min);
        let start = ms_to_frames(settings.start_ms).clamp(min, max);
        Self {
            settings,
            target_frames: start,
            mode: Mode::Idle,
            resync: false,
            next_seq: 0,
            slots: VecDeque::new(),
            end_seq: None,
            missing_run: 0,
            ran_dry: false,
            stats: JitterStats::default(),
            jitter_units: 0.0,
            prev: None,
            spurt_ended: false,
            spare: Vec::new(),
        }
    }

    fn min_frames(&self) -> u32 {
        ms_to_frames(self.settings.min_ms)
    }

    fn max_frames(&self) -> u32 {
        ms_to_frames(self.settings.max_ms).max(self.min_frames())
    }

    /// Current target depth in milliseconds.
    #[must_use]
    pub fn target_ms(&self) -> u32 {
        // Saturating: `max_ms` near `u32::MAX` rounds up to a frame count whose product overflows.
        // Regression: tests/tqs_sweep.rs::jitter_target_ms_does_not_overflow_for_huge_settings_regression
        self.target_frames.saturating_mul(FRAME_MS)
    }

    /// Frames between the cursor and the newest stored packet.
    #[must_use]
    pub fn depth_frames(&self) -> usize {
        self.slots.len()
    }

    /// Counters, with the current jitter estimate filled in.
    #[must_use]
    pub fn stats(&self) -> JitterStats {
        JitterStats {
            jitter_ms: (self.jitter_units * 1000.0 / f64::from(TIMESTAMP_RATE)) as f32,
            ..self.stats
        }
    }

    /// Whether the buffer holds nothing and waits for a talk spurt.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.mode == Mode::Idle
    }

    /// Drops everything and returns to idle (decoder flush, `reset` flag).
    pub fn reset(&mut self) {
        self.mode = Mode::Idle;
        self.resync = false;
        self.drain_slots();
        self.end_seq = None;
        self.missing_run = 0;
        self.ran_dry = false;
        self.prev = None;
        self.spurt_ended = false;
    }

    /// Whether a talk spurt's end frame was played since the last call. The speaker resets its
    /// decoder then, so a loss at the start of the next spurt is not concealed from the previous
    /// utterance (audit-gameplay-muse M3).
    pub(crate) fn take_spurt_end(&mut self) -> bool {
        std::mem::take(&mut self.spurt_ended)
    }

    fn update_jitter(&mut self, arrival_frame: u64, timestamp: u32) {
        if let Some((prev_arrival, prev_ts)) = self.prev {
            let darr = (arrival_frame.wrapping_sub(prev_arrival) as i64) * i64::from(FRAME_TIMESTAMP_STEP);
            let dts = i64::from(timestamp.wrapping_sub(prev_ts) as i32);
            let d = (darr - dts).abs() as f64;
            self.jitter_units += (d - self.jitter_units) / 16.0;
        }
        self.prev = Some((arrival_frame, timestamp));
    }

    fn anchor(&mut self, seq: u16, spurt_start: bool) {
        // Depth follows the jitter estimate at talk-spurt boundaries only (spec § 2.9).
        if spurt_start && self.stats.received >= JITTER_WARMUP {
            let want_ms = 2.5 * self.stats().jitter_ms + FRAME_MS as f32;
            let frames = (want_ms / FRAME_MS as f32).ceil() as u32;
            self.target_frames = frames.clamp(self.min_frames(), self.max_frames());
        }
        self.next_seq = seq;
        self.drain_slots();
        self.end_seq = None;
        self.missing_run = 0;
        self.ran_dry = false;
        self.resync = false;
        self.mode = Mode::Buffering;
    }

    /// Stores a packet. `arrival_frame` is the caller's frame counter when it arrived.
    pub fn insert(
        &mut self,
        seq: u16,
        timestamp: u32,
        end_of_talk: bool,
        payload: &[u8],
        arrival_frame: u64,
    ) -> InsertOutcome {
        self.update_jitter(arrival_frame, timestamp);
        self.store(seq, end_of_talk, payload)
    }

    /// Stores the second frame of a two-frame packet: it arrived with the first, so it says nothing
    /// about network jitter.
    pub fn insert_following(&mut self, seq: u16, end_of_talk: bool, payload: &[u8]) -> InsertOutcome {
        self.store(seq, end_of_talk, payload)
    }

    /// Offers a spare copy of an earlier frame. It fills a hole ahead of the cursor; a frame already
    /// held or already played is the normal case and counts as nothing.
    pub fn insert_spare(&mut self, seq: u16, payload: &[u8]) {
        if self.mode == Mode::Idle {
            return;
        }
        let diff = seq_diff(seq, self.next_seq);
        if diff < 0 {
            return;
        }
        let hole = self.slots.get(diff as usize).is_some_and(Option::is_none);
        if hole {
            let buf = self.take_payload(payload);
            if let Some(slot) = self.slots.get_mut(diff as usize) {
                *slot = Some(buf);
                self.stats.spare_filled += 1;
            }
        }
    }

    fn store(&mut self, seq: u16, end_of_talk: bool, payload: &[u8]) -> InsertOutcome {
        let mut outcome = InsertOutcome::Accepted;
        if self.mode == Mode::Idle {
            self.anchor(seq, true);
        } else if self.resync && self.slots.iter().all(Option::is_none) {
            self.anchor(seq, false);
        } else {
            let diff = seq_diff(seq, self.next_seq);
            if diff < 0 {
                self.stats.late += 1;
                return InsertOutcome::Late;
            }
            if diff >= WINDOW_FRAMES {
                self.stats.resyncs += 1;
                self.anchor(seq, false);
                outcome = InsertOutcome::Resynced;
            }
        }
        let idx = seq_diff(seq, self.next_seq) as usize;
        if self.slots.len() <= idx {
            self.slots.resize(idx + 1, None);
        }
        if self.slots[idx].is_some() {
            self.stats.duplicates += 1;
            return InsertOutcome::Duplicate;
        }
        self.slots[idx] = Some(self.take_payload(payload));
        self.stats.received += 1;
        self.ran_dry = false;
        if end_of_talk {
            self.end_seq = Some(seq);
        }
        outcome
    }

    fn take_payload(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut buf = self.spare.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(payload);
        buf
    }

    fn keep_spare(&mut self, buf: Vec<u8>) {
        if self.spare.len() < 64 {
            self.spare.push(buf);
        }
    }

    fn drain_slots(&mut self) {
        while let Some(slot) = self.slots.pop_front() {
            if let Some(buf) = slot {
                self.keep_spare(buf);
            }
        }
    }

    /// [`pull`](Self::pull) that copies a frame into `scratch` and recycles the slot buffer.
    pub(crate) fn pull_into(&mut self, scratch: &mut Vec<u8>) -> PullKind {
        match self.pull() {
            Playout::Buffering => PullKind::Buffering,
            Playout::Conceal => PullKind::Conceal,
            Playout::Silence => PullKind::Silence,
            Playout::Finished => PullKind::Finished,
            Playout::Frame(buf) => {
                scratch.clear();
                scratch.extend_from_slice(&buf);
                self.keep_spare(buf);
                PullKind::Frame
            }
            Playout::Recover(buf) => {
                scratch.clear();
                scratch.extend_from_slice(&buf);
                self.keep_spare(buf);
                PullKind::Recover
            }
        }
    }

    /// Produces the output for the next 20 ms frame.
    pub fn pull(&mut self) -> Playout {
        match self.mode {
            Mode::Idle => return Playout::Finished,
            Mode::Buffering => {
                let ready = self.slots.len() as u32 >= self.target_frames
                    || (self.end_seq.is_some() && !self.slots.is_empty());
                if !ready {
                    return if self.resync {
                        self.dry_output()
                    } else {
                        Playout::Buffering
                    };
                }
                self.mode = Mode::Playing;
            }
            Mode::Playing => {}
        }
        let Some(front) = self.slots.pop_front() else {
            // Nothing buffered at or after the cursor. Time keeps moving: the frame counts as
            // missing (concealed) and a packet that was only slow arrives with a sequence number
            // still ahead of the cursor. Only a longer silence is an underrun.
            if self.missing_run >= MAX_CONCEALED {
                return self.run_dry();
            }
            self.next_seq = self.next_seq.wrapping_add(1);
            self.missing_run += 1;
            self.stats.concealed += 1;
            return Playout::Conceal;
        };
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let finished = self.end_seq == Some(seq);
        match front {
            Some(payload) => {
                self.missing_run = 0;
                self.stats.played += 1;
                if finished {
                    self.end_seq = None;
                    self.prev = None;
                    self.spurt_ended = true;
                    if self.slots.iter().any(Option::is_some) {
                        // The next utterance already arrived behind the end frame: keep it and
                        // prefill again instead of discarding its first frames.
                        self.mode = Mode::Buffering;
                    } else {
                        self.mode = Mode::Idle;
                        self.slots.clear();
                    }
                }
                Playout::Frame(payload)
            }
            None => {
                self.missing_run += 1;
                if self.missing_run > MAX_CONCEALED {
                    self.stats.silenced += 1;
                    Playout::Silence
                } else if let Some(Some(next)) = self.slots.front() {
                    self.stats.recovered += 1;
                    Playout::Recover(next.clone())
                } else {
                    self.stats.concealed += 1;
                    Playout::Conceal
                }
            }
        }
    }

    fn dry_output(&mut self) -> Playout {
        if self.missing_run >= MAX_CONCEALED {
            self.stats.silenced += 1;
            Playout::Silence
        } else {
            self.missing_run += 1;
            self.stats.concealed += 1;
            Playout::Conceal
        }
    }

    fn run_dry(&mut self) -> Playout {
        // Nothing buffered at all: an underrun, not a loss. Grow quickly and re-anchor on the
        // next packet.
        if !self.ran_dry {
            self.ran_dry = true;
            self.stats.underruns += 1;
            self.target_frames = (self.target_frames + 1).min(self.max_frames());
        }
        self.mode = Mode::Buffering;
        self.resync = true;
        self.dry_output()
    }
}
