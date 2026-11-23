// SPDX-License-Identifier: MPL-2.0
//! The single-slot frame mailbox (spec 58 § 2.4): the XR frame loop publishes the latest frame
//! state; extraction reads the freshest slot without waiting. A sequence lock over atomics: the
//! writer never blocks, a reader that races a write retries a bounded number of times and then
//! reports "no new slot" (the renderer repeats the old frame's layers and counts a stale frame).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering, fence};

use regia_app::prelude::*;

/// What `wait_frame` predicted for one XR frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrFrameState {
    /// The frame's index (frames waited since the session began).
    pub frame_index: u64,
    /// Predicted display time, nanoseconds on the runtime clock.
    pub predicted_display_time_ns: i64,
    /// Predicted display period, nanoseconds.
    pub period_ns: i64,
    /// Whether the runtime wants the frame rendered.
    pub should_render: bool,
}

#[derive(Debug, Default)]
struct Slot {
    seq: AtomicU64,
    frame_index: AtomicU64,
    time: AtomicI64,
    period: AtomicI64,
    should_render: AtomicU64,
}

/// Retries a reader makes when it races a write.
const READ_RETRIES: u32 = 8;

/// The mailbox. Clones share one slot (one clone for the frame thread, one in the ECS world).
#[derive(Debug, Clone, Default, Resource)]
pub struct XrFrameMailbox {
    slot: Arc<Slot>,
    last_read: Option<u64>,
}

impl XrFrameMailbox {
    /// An empty mailbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes `state`, replacing the slot. Never blocks.
    pub fn publish(&self, state: XrFrameState) {
        let s = &self.slot;
        let seq = s.seq.load(Ordering::Relaxed);
        s.seq.store(seq.wrapping_add(1), Ordering::Relaxed); // odd: write in progress
        fence(Ordering::Release);
        s.frame_index.store(state.frame_index, Ordering::Relaxed);
        s.time.store(state.predicted_display_time_ns, Ordering::Relaxed);
        s.period.store(state.period_ns, Ordering::Relaxed);
        s.should_render
            .store(u64::from(state.should_render), Ordering::Relaxed);
        s.seq.store(seq.wrapping_add(2), Ordering::Release); // even: published
    }

    /// The slot's content, or `None` when nothing was published (or a write kept racing).
    pub fn peek(&self) -> Option<XrFrameState> {
        let s = &self.slot;
        for _ in 0..READ_RETRIES {
            let before = s.seq.load(Ordering::Acquire);
            if before == 0 {
                return None;
            }
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let state = XrFrameState {
                frame_index: s.frame_index.load(Ordering::Relaxed),
                predicted_display_time_ns: s.time.load(Ordering::Relaxed),
                period_ns: s.period.load(Ordering::Relaxed),
                should_render: s.should_render.load(Ordering::Relaxed) != 0,
            };
            fence(Ordering::Acquire);
            if s.seq.load(Ordering::Relaxed) == before {
                return Some(state);
            }
        }
        None
    }

    /// The slot when it holds a frame this reader has not seen; `None` means "no new slot since the
    /// last read": repeat the old frame.
    pub fn take_fresh(&mut self) -> Option<XrFrameState> {
        let state = self.peek()?;
        if self.last_read == Some(state.frame_index) {
            return None;
        }
        self.last_read = Some(state.frame_index);
        Some(state)
    }

    /// Forgets what this reader saw (a new session restarts frame indices at zero).
    pub fn reset_reader(&mut self) {
        self.last_read = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(i: u64) -> XrFrameState {
        XrFrameState {
            frame_index: i,
            predicted_display_time_ns: 1000 * (i as i64 + 1),
            period_ns: 1000,
            should_render: i.is_multiple_of(2),
        }
    }

    #[test]
    fn the_freshest_slot_wins_and_a_repeat_is_not_fresh() {
        let writer = XrFrameMailbox::new();
        let mut reader = writer.clone();
        assert_eq!(reader.take_fresh(), None, "empty");
        writer.publish(state(0));
        writer.publish(state(1));
        assert_eq!(reader.take_fresh(), Some(state(1)), "older slot overwritten");
        assert_eq!(reader.take_fresh(), None, "no new slot");
        assert_eq!(reader.peek(), Some(state(1)));
        writer.publish(state(2));
        assert_eq!(reader.take_fresh(), Some(state(2)));
        reader.reset_reader();
        assert_eq!(reader.take_fresh(), Some(state(2)));
    }

    #[test]
    fn a_reader_on_another_thread_sees_only_whole_slots() {
        let writer = XrFrameMailbox::new();
        let reader = writer.clone();
        let handle = std::thread::spawn(move || {
            for i in 0..20_000 {
                writer.publish(state(i));
            }
        });
        let mut seen = 0u64;
        while !handle.is_finished() {
            if let Some(s) = reader.peek() {
                assert_eq!(s, state(s.frame_index), "torn read");
                seen += 1;
            }
        }
        handle.join().expect("writer");
        assert_eq!(reader.peek(), Some(state(19_999)));
        let _ = seen;
    }
}
