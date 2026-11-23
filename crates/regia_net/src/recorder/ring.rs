// SPDX-License-Identifier: MPL-2.0
//! Ring mode (spec 103 § 7): the newest whole segments in one fixed block of memory, for kill-cams and instant
//! replays. Chunks are stored in the file encoding, so an export is a header, the bytes, and an end chunk.

use std::collections::VecDeque;

use super::format::{CHUNK_HEADER_BYTES, NetDemoError};
use crate::model::NetTick;

/// The most segments a ring tracks, whatever its window asks for. The window's oldest history goes first past this.
pub const MAX_RING_SEGMENTS: usize = 1 << 16;

/// One segment: where its keyframe chunk starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    /// The logical byte offset of the keyframe chunk (bytes ever written, not a buffer position).
    start: u64,
    tick: NetTick,
}

/// What a ring holds.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RingStatus {
    /// The memory bound.
    pub max_bytes: usize,
    /// Bytes in use.
    pub used_bytes: usize,
    /// The largest `used_bytes` so far.
    pub peak_bytes: usize,
    /// Whole segments held.
    pub segments: usize,
    /// The oldest tick held.
    pub first_tick: Option<NetTick>,
    /// The newest tick held.
    pub last_tick: Option<NetTick>,
    /// Segments larger than the ring: it emptied and waited for the next keyframe.
    pub overflows: u64,
}

/// See the module docs.
#[derive(Debug)]
pub struct Ring {
    buf: Vec<u8>,
    /// The bytes ever written; `head % buf.len()` is where the next byte goes.
    head: u64,
    /// The oldest byte kept, in the same numbering.
    tail: u64,
    segments: VecDeque<Segment>,
    max_segments: usize,
    window_ticks: u32,
    keyframe_ticks: u32,
    waiting_for_keyframe: bool,
    last_tick: Option<NetTick>,
    peak: usize,
    overflows: u64,
}

impl Ring {
    /// A ring of exactly `max_bytes`, keeping `window_ticks` of history, with keyframes every `keyframe_ticks`.
    ///
    /// # Errors
    /// [`NetDemoError::ConfigInvalid`] when the memory is not there.
    pub fn new(max_bytes: usize, window_ticks: u32, keyframe_ticks: u32) -> Result<Self, NetDemoError> {
        let max_segments = ((window_ticks / keyframe_ticks.max(1)) as usize)
            .saturating_add(3)
            .min(MAX_RING_SEGMENTS);
        let mut buf = Vec::new();
        buf.try_reserve_exact(max_bytes)
            .map_err(|_| NetDemoError::ConfigInvalid("max_bytes: that much memory is not available"))?;
        buf.resize(max_bytes, 0);
        Ok(Self {
            buf,
            head: 0,
            tail: 0,
            segments: VecDeque::with_capacity(max_segments),
            max_segments,
            window_ticks,
            keyframe_ticks: keyframe_ticks.max(1),
            waiting_for_keyframe: true,
            last_tick: None,
            peak: 0,
            overflows: 0,
        })
    }

    /// Whether a keyframe at `tick` starts a segment here: not while the newest segment is younger than the ring's own
    /// interval, so a file with a shorter one does not use up its segments (spec 103 § 7).
    #[must_use]
    pub fn wants_keyframe(&self, tick: NetTick) -> bool {
        self.waiting_for_keyframe
            || self
                .segments
                .back()
                .is_none_or(|s| tick.0.saturating_sub(s.tick.0) >= self.keyframe_ticks)
    }

    /// Forgets what it holds and waits for a keyframe (the session changed level).
    pub fn restart(&mut self) {
        self.segments.clear();
        self.tail = self.head;
        self.waiting_for_keyframe = true;
    }

    /// Bytes in use.
    #[must_use]
    pub fn used(&self) -> usize {
        (self.head - self.tail) as usize
    }

    /// The state for `net.replay_state`.
    #[must_use]
    pub fn status(&self) -> RingStatus {
        RingStatus {
            max_bytes: self.buf.len(),
            used_bytes: self.used(),
            peak_bytes: self.peak,
            segments: self.segments.len(),
            first_tick: self.segments.front().map(|s| s.tick),
            last_tick: self.last_tick.filter(|_| !self.segments.is_empty()),
            overflows: self.overflows,
        }
    }

    /// Whether the next chunk would be dropped until a keyframe comes.
    #[must_use]
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    fn drop_oldest_segment(&mut self) {
        self.segments.pop_front();
        match self.segments.front() {
            Some(next) => self.tail = next.start,
            None => {
                self.tail = self.head;
                self.waiting_for_keyframe = true;
            }
        }
    }

    fn copy_in(&mut self, bytes: &[u8]) {
        let cap = self.buf.len();
        if cap == 0 {
            return;
        }
        let at = (self.head % cap as u64) as usize;
        let first = bytes.len().min(cap - at);
        let (a, b) = bytes.split_at(first);
        if let Some(dst) = self.buf.get_mut(at..at + first) {
            dst.copy_from_slice(a);
        }
        if let Some(dst) = self.buf.get_mut(..b.len()) {
            dst.copy_from_slice(b);
        }
        self.head += bytes.len() as u64;
    }

    /// Stores one encoded chunk. Keyframes start segments; history beyond the window and anything past the byte bound
    /// leaves whole segments at a time.
    pub fn push(&mut self, bytes: &[u8], keyframe: bool, tick: NetTick) {
        let cap = self.buf.len();
        if bytes.len() > cap {
            // This segment cannot fit: nothing older is of use to it either.
            self.segments.clear();
            self.tail = self.head;
            self.waiting_for_keyframe = true;
            self.overflows += 1;
            return;
        }
        if keyframe {
            // The second-oldest segment still reaches back to the window: the oldest is no longer needed.
            while self
                .segments
                .get(1)
                .is_some_and(|next| tick.0.saturating_sub(next.tick.0) >= self.window_ticks)
            {
                self.drop_oldest_segment();
            }
            while self.segments.len() >= self.max_segments {
                self.drop_oldest_segment();
            }
        } else if self.waiting_for_keyframe {
            return;
        }
        while self.used() + bytes.len() > cap {
            let was_only = self.segments.len() <= 1;
            self.drop_oldest_segment();
            if was_only {
                // The segment being written outgrew the ring (a keyframe that starts a new one does not count:
                // it stands alone now, which is what it is for).
                if !keyframe {
                    self.overflows += 1;
                    return;
                }
                break;
            }
        }
        if keyframe {
            self.waiting_for_keyframe = false;
            self.segments.push_back(Segment {
                start: self.head,
                tick,
            });
        }
        self.copy_in(bytes);
        self.last_tick = Some(tick);
        self.peak = self.peak.max(self.used());
    }

    /// The bytes of the newest segment starting at or before `from`, as up to two slices, with its start tick. `None` when
    /// the ring holds nothing. A `from` older than the ring gives everything.
    #[must_use]
    pub fn slices_from(&self, from: NetTick) -> Option<(NetTick, &[u8], &[u8])> {
        let oldest = self.segments.front()?;
        let segment = self
            .segments
            .iter()
            .rev()
            .find(|s| s.tick <= from)
            .unwrap_or(oldest);
        let cap = self.buf.len() as u64;
        let at = (segment.start % cap) as usize;
        let len = (self.head - segment.start) as usize;
        let first = len.min(self.buf.len() - at);
        let a = self.buf.get(at..at + first)?;
        let b = self.buf.get(..len - first)?;
        Some((segment.tick, a, b))
    }
}

/// Counts the chunks in the ring's byte slices: `(tick chunks, keyframes, last tick)`. The ring wrote them, so a bad
/// header is a bug and ends the count.
pub(crate) fn count_chunks(a: &[u8], b: &[u8]) -> (u64, u64, NetTick) {
    let byte_at = |i: usize| -> Option<u8> {
        if i < a.len() {
            a.get(i).copied()
        } else {
            b.get(i - a.len()).copied()
        }
    };
    let total = a.len() + b.len();
    let mut at = 0;
    let (mut ticks, mut keys, mut last) = (0u64, 0u64, NetTick(0));
    while at + CHUNK_HEADER_BYTES <= total {
        let word = |from: usize| -> Option<u32> {
            Some(u32::from_le_bytes([
                byte_at(from)?,
                byte_at(from + 1)?,
                byte_at(from + 2)?,
                byte_at(from + 3)?,
            ]))
        };
        let (Some(kind), Some(tick), Some(len)) = (byte_at(at), word(at + 1), word(at + 5)) else {
            break;
        };
        match kind {
            1 => keys += 1,
            2 => {
                ticks += 1;
                last = NetTick(tick);
            }
            _ => {}
        }
        at += CHUNK_HEADER_BYTES + len as usize;
    }
    (ticks, keys, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize) -> Vec<u8> {
        vec![7; n]
    }

    #[test]
    fn it_never_holds_more_than_its_bound_and_drops_whole_segments() {
        let mut ring = Ring::new(1000, 600, 100).expect("a ring");
        ring.push(&chunk(100), true, NetTick(0));
        for t in 1..50 {
            ring.push(&chunk(50), false, NetTick(t));
            assert!(ring.used() <= 1000, "{}", ring.used());
        }
        assert!(ring.status().peak_bytes <= 1000);
        // The first segment outgrew the ring: it is gone, and nothing is kept until a keyframe.
        assert!(ring.status().overflows > 0);
        assert_eq!(ring.status().segments, 0);
        ring.push(&chunk(50), false, NetTick(60));
        assert_eq!(ring.used(), 0, "waits for a keyframe");
        ring.push(&chunk(100), true, NetTick(61));
        assert_eq!(ring.used(), 100);
        assert_eq!(ring.status().first_tick, Some(NetTick(61)));
    }

    #[test]
    fn a_chunk_larger_than_the_ring_empties_it_and_counts() {
        let mut ring = Ring::new(100, 600, 100).expect("a ring");
        ring.push(&chunk(50), true, NetTick(0));
        ring.push(&chunk(101), true, NetTick(10));
        assert_eq!(ring.used(), 0);
        assert_eq!(ring.status().overflows, 1);
        ring.push(&chunk(50), true, NetTick(20));
        assert_eq!(ring.used(), 50);
    }

    #[test]
    fn history_leaves_when_the_next_segment_reaches_the_window() {
        let mut ring = Ring::new(10_000, 100, 50).expect("a ring");
        for key in 0..6u32 {
            ring.push(&chunk(10), true, NetTick(key * 50));
        }
        // Newest keyframe at 250, window 100: the segment at 150 reaches back to it and is the oldest kept.
        assert_eq!(ring.status().first_tick, Some(NetTick(150)));
    }

    /// Spec 103 § 7: a file with keyframes ten times as frequent leaves the ring's segments, and so its history, alone.
    #[test]
    fn a_shorter_interval_elsewhere_does_not_cut_the_ring_short() {
        let mut ring = Ring::new(1 << 20, 3000, 500).expect("a ring");
        for t in 0..10_000u32 {
            let key = t.is_multiple_of(50);
            if key && ring.wants_keyframe(NetTick(t)) {
                ring.push(&chunk(10), true, NetTick(t));
            }
            ring.push(&chunk(10), false, NetTick(t));
        }
        let held = ring.status();
        let span = held.last_tick.expect("held").0 - held.first_tick.expect("held").0;
        assert!(span >= 3000, "the window is kept: {span} ticks");
        assert!(held.segments <= 3000 / 500 + 3, "{}", held.segments);
    }

    #[test]
    fn asking_for_too_much_is_refused_and_a_long_window_tracks_a_bounded_list() {
        let code = Ring::new(usize::MAX, 100, 1).expect_err("refused").code();
        assert_eq!(code, "recorder_config_invalid");
        let ring = Ring::new(4096, u32::MAX, 1).expect("a ring");
        assert_eq!(ring.max_segments, MAX_RING_SEGMENTS);
    }

    #[test]
    fn a_restart_forgets_what_it_holds_and_waits_for_a_keyframe() {
        let mut ring = Ring::new(1000, 600, 100).expect("a ring");
        ring.push(&chunk(100), true, NetTick(0));
        ring.restart();
        assert_eq!(ring.used(), 0);
        assert_eq!(ring.status().segments, 0);
        ring.push(&chunk(10), false, NetTick(1));
        assert_eq!(ring.used(), 0, "waits for a keyframe");
        assert!(ring.wants_keyframe(NetTick(2)));
        ring.push(&chunk(10), true, NetTick(2));
        assert_eq!(ring.status().first_tick, Some(NetTick(2)));
    }

    #[test]
    fn a_read_across_the_wrap_keeps_the_bytes_in_order() {
        let mut ring = Ring::new(100, 10_000, 100).expect("a ring");
        ring.push(&[1; 60], true, NetTick(0));
        ring.push(&[2; 30], true, NetTick(1));
        ring.push(&[3; 30], true, NetTick(2)); // drops the first segment and wraps
        let (tick, a, b) = ring.slices_from(NetTick(1)).expect("held");
        assert_eq!(tick, NetTick(1));
        let bytes: Vec<u8> = a.iter().chain(b).copied().collect();
        assert_eq!(bytes.len(), 60);
        assert!(bytes[..30].iter().all(|x| *x == 2));
        assert!(bytes[30..].iter().all(|x| *x == 3));
    }
}
