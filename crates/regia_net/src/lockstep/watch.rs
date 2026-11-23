// SPDX-License-Identifier: MPL-2.0
//! Late stamping: a seat that sits on its inputs (spec 120 § 7.3; ADR-8209).
//!
//! An honest seat samples the input of capture tick `c` when it steps frame `c - 1` and sends it
//! in the datagram that first acknowledges that frame. At the host the input's first arrival and
//! the first acknowledgement that reaches `c` are therefore the same datagram: the **hold**
//! between the two is zero. That is true whatever the link does: both ride the same datagram, so
//! latency, jitter and asymmetry cancel, and after a loss the next datagram carries both again.
//!
//! A modified client can sample later than its capture tick wherever the input delay leaves
//! room: it acknowledges frames as they come and sends each input just before the frame that
//! needs it is built, so it reacts to newer state than everyone else. At the host that is a hold
//! of several ticks on nearly every input, each arriving at the edge of its deadline. A client
//! that holds its acknowledgements back too shows the same time as **acknowledgement lag**: its
//! frames are acknowledged later than the transport's own round trip on the same connection
//! explains (the application feeds that round trip in).
//!
//! [`StampWatch`] keeps, per seat, the hold and the **margin** (deadline minus arrival) of its
//! clean first arrivals over the last [`WATCH_WINDOW_MS`] and calls the seat suspect when nine in
//! ten of them were held at least [`SUSPECT_HOLD_TICKS`] ticks and the median margin is no larger
//! than that hold: it waits at least as long as the slack it leaves. An honest seat on a slow or
//! lossy link has a small margin and no hold; an application that samples a frame late has a
//! hold of a tick and keeps its margin. The verdict is a flag for people to read. Nothing is
//! removed for it.
//!
//! What it cannot see: a client that delays every packet it sends, the transport's
//! acknowledgements included, is a slow uplink to every measurement. What such a client gains is
//! bounded by the input delay minus its real round trip, and it is then the seat that sets the
//! delay, which the status already shows (`delay_need_ticks`).

use std::collections::VecDeque;

/// How far back a verdict looks, milliseconds.
pub(crate) const WATCH_WINDOW_MS: u64 = 10_000;
/// Clean first arrivals the window must hold before it says anything.
pub(crate) const WATCH_MIN_SAMPLES: usize = 240;
/// A seat is suspect when nine in ten of its inputs were held at least this many ticks.
pub(crate) const SUSPECT_HOLD_TICKS: u64 = 2;
/// Samples kept at most.
const MAX_SAMPLES: usize = 1024;
/// Acknowledgement steps kept at most.
const MAX_ACKS: usize = 256;

/// What a verdict changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatchChange {
    /// The seat is suspect now (`false`: no longer).
    pub(crate) suspect: bool,
    /// The hold nine in ten of its inputs reached, microseconds.
    pub(crate) hold_us: u64,
    /// Its median margin, microseconds.
    pub(crate) margin_us: i64,
}

/// One seat's input timing against its own acknowledgements.
#[derive(Debug, Clone, Default)]
pub(crate) struct StampWatch {
    /// `(arrival ms, hold us, margin us)` of clean first arrivals, oldest first.
    samples: VecDeque<(u64, u64, i64)>,
    /// `(ack_next, first arrival ms)`: when the seat first acknowledged every frame before
    /// `ack_next`; ascending in both.
    acks: VecDeque<(u64, u64)>,
    /// `(arrival ms, microseconds from a frame's build to its first acknowledgement)`.
    trips: VecDeque<(u64, u64)>,
    /// The transport's smoothed round trip on the seat's connection, microseconds.
    transport_rtt_us: Option<u64>,
    scratch: Vec<i64>,
    /// The last verdict.
    pub(crate) suspect: bool,
    /// Times the seat became suspect.
    pub(crate) flagged: u32,
    /// The hold nine in ten of the window's inputs reached (acknowledgement lag included),
    /// microseconds; `None` without enough samples.
    pub(crate) hold_p10_us: Option<u64>,
    /// The window's median margin, microseconds; `None` without enough samples.
    pub(crate) margin_p50_us: Option<i64>,
}

/// The value at `per_cent` of `values` (nearest rank); sorts them.
fn rank(values: &mut [i64], per_cent: usize) -> Option<i64> {
    values.sort_unstable();
    let index = (values.len() * per_cent).div_ceil(100).saturating_sub(1);
    values.get(index).copied()
}

impl StampWatch {
    /// The transport's round trip on the seat's connection, milliseconds.
    pub(crate) fn note_transport_rtt(&mut self, rtt_ms: u64) {
        self.transport_rtt_us = Some(rtt_ms.saturating_mul(1000));
    }

    /// The seat's acknowledgement stands at `ack_next` (it has every frame before it) at
    /// `now_ms`; frame `ack_next - 1` was built at `built_us`.
    pub(crate) fn on_ack(&mut self, ack_next: u64, now_ms: u64, built_us: u64) {
        if ack_next == 0 || self.acks.back().is_some_and(|(newest, _)| *newest >= ack_next) {
            return;
        }
        if self.acks.len() >= MAX_ACKS {
            self.acks.pop_front();
        }
        self.acks.push_back((ack_next, now_ms));
        if self.trips.len() >= MAX_SAMPLES {
            self.trips.pop_front();
        }
        self.trips
            .push_back((now_ms, now_ms.saturating_mul(1000).saturating_sub(built_us)));
    }

    /// A clean first arrival, at `now_ms`, of the input sampled at capture tick `capture`,
    /// `margin_us` before the frame it is stamped for is built.
    pub(crate) fn on_input(&mut self, capture: u64, margin_us: i64, now_ms: u64) {
        // The capture opened when the seat had frame `capture - 1`. An acknowledgement older than
        // the ring says nothing (the seat was far behind), and capture 0 has no frame before it.
        let Some(acked_ms) = self
            .acks
            .iter()
            .find(|(ack_next, _)| *ack_next >= capture)
            .map(|(_, at)| *at)
        else {
            return;
        };
        if capture == 0 || self.acks.front().is_some_and(|(oldest, _)| *oldest > capture) {
            return;
        }
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        let hold_us = now_ms.saturating_sub(acked_ms).saturating_mul(1000);
        self.samples.push_back((now_ms, hold_us, margin_us));
    }

    /// Judges the window that ends at `now_ms`. Returns the change when the verdict changed.
    pub(crate) fn judge(&mut self, now_ms: u64, tick_us: u64) -> Option<WatchChange> {
        let horizon = now_ms.saturating_sub(WATCH_WINDOW_MS);
        while self.samples.front().is_some_and(|(at, _, _)| *at < horizon) {
            self.samples.pop_front();
        }
        while self.trips.front().is_some_and(|(at, _)| *at < horizon) {
            self.trips.pop_front();
        }
        let was = self.suspect;
        if self.samples.len() < WATCH_MIN_SAMPLES {
            self.suspect = false;
            self.hold_p10_us = None;
            self.margin_p50_us = None;
            return was.then_some(WatchChange {
                suspect: false,
                hold_us: 0,
                margin_us: 0,
            });
        }
        self.scratch.clear();
        self.scratch.extend(
            self.samples
                .iter()
                .map(|(_, hold, _)| i64::try_from(*hold).unwrap_or(i64::MAX)),
        );
        let hold = rank(&mut self.scratch, 10).unwrap_or(0);
        self.scratch.clear();
        self.scratch
            .extend(self.samples.iter().map(|(_, _, margin)| *margin));
        let margin = rank(&mut self.scratch, 50).unwrap_or(0);
        // Frames acknowledged later than the connection's own round trip explains: time the seat
        // had the frame and did not say so. One tick is the seat's own frame.
        let lag = match self.transport_rtt_us {
            Some(rtt_us) if self.trips.len() >= WATCH_MIN_SAMPLES => {
                let allowed = i64::try_from(rtt_us.saturating_add(tick_us)).unwrap_or(i64::MAX);
                self.scratch.clear();
                self.scratch.extend(
                    self.trips
                        .iter()
                        .map(|(_, trip)| i64::try_from(*trip).unwrap_or(i64::MAX).saturating_sub(allowed)),
                );
                rank(&mut self.scratch, 10).unwrap_or(0).max(0)
            }
            _ => 0,
        };
        let held = hold.saturating_add(lag).max(0);
        let limit = i64::try_from(SUSPECT_HOLD_TICKS.saturating_mul(tick_us)).unwrap_or(i64::MAX);
        self.suspect = held >= limit && margin <= held;
        self.hold_p10_us = Some(held as u64);
        self.margin_p50_us = Some(margin);
        if self.suspect && !was {
            self.flagged += 1;
        }
        (self.suspect != was).then_some(WatchChange {
            suspect: self.suspect,
            hold_us: held as u64,
            margin_us: margin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_US: u64 = 16_666;

    /// Feeds `watch` one input per tick for `ticks` ticks: the seat acknowledges frame `c - 1`
    /// `rtt_ms` after it is built and its input for capture `c` arrives `hold_ms` after that,
    /// stamped `delay` ticks ahead.
    fn feed(watch: &mut StampWatch, ticks: u64, rtt_ms: u64, hold_ms: u64, delay: u64) -> u64 {
        let mut pending: VecDeque<(u64, u64, i64)> = VecDeque::new();
        let mut now = 0;
        for c in 1..=ticks {
            let built_us = (c - 1) * TICK_US;
            let acked_ms = built_us / 1000 + rtt_ms;
            let arrives_ms = acked_ms + hold_ms;
            let deadline_us = (c + delay) * TICK_US;
            pending.push_back((arrives_ms, c, deadline_us as i64 - (arrives_ms * 1000) as i64));
            watch.on_ack(c, acked_ms, built_us);
            while pending.front().is_some_and(|(at, _, _)| *at <= acked_ms) {
                let (at, capture, margin) = pending.pop_front().expect("checked");
                watch.on_input(capture, margin, at);
            }
            now = acked_ms;
        }
        now
    }

    #[test]
    fn an_honest_seat_has_no_hold_whatever_its_round_trip() {
        for rtt_ms in [4u64, 40, 90] {
            let mut watch = StampWatch::default();
            let now = feed(&mut watch, 600, rtt_ms, 0, 6);
            assert_eq!(watch.judge(now, TICK_US), None, "rtt {rtt_ms}");
            assert!(!watch.suspect);
            assert_eq!(watch.hold_p10_us, Some(0), "rtt {rtt_ms}");
        }
        // 90 ms of round trip under a 6-tick delay: its inputs arrive near the deadline, and it
        // is still not suspect: it did not wait.
        let mut watch = StampWatch::default();
        let now = feed(&mut watch, 600, 90, 0, 6);
        watch.judge(now, TICK_US);
        assert!(watch.margin_p50_us.is_some_and(|m| m < 2 * TICK_US as i64));
        assert!(!watch.suspect);
    }

    #[test]
    fn a_seat_that_waits_for_the_deadline_is_suspect_and_cleared_when_it_stops() {
        let mut watch = StampWatch::default();
        // 10 ms of round trip, 6 ticks of delay: the input could arrive 90 ms before its frame.
        // It arrives 80 ms after the seat acknowledged the frame that opened its capture.
        let now = feed(&mut watch, 600, 10, 80, 6);
        let change = watch.judge(now, TICK_US).expect("the verdict changed");
        assert!(change.suspect && watch.suspect);
        assert_eq!(change.hold_us, 80_000);
        assert!(change.margin_us <= 80_000, "{change:?}");
        assert_eq!(watch.flagged, 1);
        assert_eq!(watch.judge(now, TICK_US), None, "no change, no second event");
        // The same seat plays honestly for the length of the window: the flag clears.
        let mut honest = StampWatch::default();
        feed(&mut honest, 700, 10, 0, 6);
        watch.samples = honest
            .samples
            .iter()
            .map(|(at, h, m)| (at + now, *h, *m))
            .collect();
        let later = now + 700 * TICK_US / 1000;
        let change = watch.judge(later, TICK_US).expect("cleared");
        assert!(!change.suspect && !watch.suspect);
        assert_eq!(watch.flagged, 1);
    }

    #[test]
    fn a_frame_of_pipeline_latency_or_too_few_samples_is_not_suspect() {
        // An application that samples one frame after it stepped: a hold of one tick.
        let mut watch = StampWatch::default();
        let now = feed(&mut watch, 600, 10, 17, 6);
        assert_eq!(watch.judge(now, TICK_US), None);
        assert_eq!(watch.hold_p10_us, Some(17_000));
        // A long hold that still leaves more margin than it took: 2 ticks held of 12.
        let mut watch = StampWatch::default();
        let now = feed(&mut watch, 600, 10, 34, 12);
        assert_eq!(watch.judge(now, TICK_US), None);
        assert!(watch.margin_p50_us.is_some_and(|m| m > 34_000));
        // Three seconds of waiting inputs are not yet a verdict.
        let mut watch = StampWatch::default();
        let now = feed(&mut watch, 180, 10, 80, 6);
        assert_eq!(watch.judge(now, TICK_US), None);
        assert_eq!((watch.suspect, watch.hold_p10_us), (false, None));
    }

    #[test]
    fn acknowledgements_held_back_show_against_the_transports_round_trip() {
        // The client holds its acknowledgement and its input alike: 80 ms on a 10 ms connection.
        let mut watch = StampWatch::default();
        watch.note_transport_rtt(10);
        let now = feed(&mut watch, 600, 90, 0, 6);
        let change = watch.judge(now, TICK_US).expect("suspect");
        assert!(change.suspect);
        // 90 ms from build to acknowledgement (arrivals are whole milliseconds, so up to one
        // less), less the 10 ms round trip and one tick.
        let expected = 90_000 - 10_000 - TICK_US;
        assert!(
            (expected - 1_000..=expected).contains(&change.hold_us),
            "{change:?}"
        );
        // Without the transport's round trip the same timing is a slow link.
        let mut blind = StampWatch::default();
        let now = feed(&mut blind, 600, 90, 0, 6);
        assert_eq!(blind.judge(now, TICK_US), None);
        // A connection whose round trip really is 90 ms is not suspect.
        let mut slow = StampWatch::default();
        slow.note_transport_rtt(90);
        let now = feed(&mut slow, 600, 90, 0, 6);
        assert_eq!(slow.judge(now, TICK_US), None);
    }
}
