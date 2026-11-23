// SPDX-License-Identifier: MPL-2.0
//! The stamp schedule: which tick an input sampled at a capture tick is meant for.
//!
//! A participant's **capture tick** `c` is the next tick it will step: it is open once frame
//! `c - 1` has been stepped (tick 0 is open at the start). The input sampled at capture tick `c`
//! is stamped `c + delay(c) + extra`, where `delay(c)` comes from the delay epochs the frames
//! carry and `extra` is a seat's own lead (`DelayPolicy::PerSeat`).
//!
//! Because every participant derives `delay(c)` from the same frames, and a capture tick opens
//! only after the frame before it was stepped, the schedule is the same function everywhere:
//!
//! * a **raise** from `d` to `d'` at capture tick `F` leaves the ticks `F + d ..= F + d' - 1`
//!   without a capture: gap ticks, the same for every seat;
//! * a **lower** makes the next captures collide with ticks already stamped: those captures are
//!   skipped ([`Stamp::Skipped`]), the same ones for every seat. No tick gets two inputs.

use std::collections::VecDeque;

/// Where the input of one capture tick goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stamp {
    /// The input is meant for this tick.
    Tick(u64),
    /// The capture has no tick of its own: the delay was lowered and its tick already has an
    /// input. A game that must not lose a press carries it into its next sample.
    Skipped,
}

/// Delay epochs and the stamps given so far.
#[derive(Debug, Clone)]
pub(crate) struct Stamper {
    /// `(first capture tick, delay)`, ascending by tick; never empty.
    epochs: VecDeque<(u64, u16)>,
    /// `(first stamped tick, delay + extra)` of each run of stamps, ascending.
    stamp_runs: VecDeque<(u64, u32)>,
    next_capture: u64,
    last_stamp: Option<u64>,
    /// Inclusive ranges of ticks no capture maps to.
    gaps: VecDeque<(u64, u64)>,
    gap_ticks: u64,
}

/// Epochs and gap ranges kept (a hostile frame stream cannot grow them without bound).
const MAX_EPOCHS: usize = 256;

impl Stamper {
    pub(crate) fn new(initial_delay: u16) -> Self {
        Self {
            epochs: VecDeque::from([(0, initial_delay)]),
            stamp_runs: VecDeque::new(),
            next_capture: 0,
            last_stamp: None,
            gaps: VecDeque::new(),
            gap_ticks: 0,
        }
    }

    /// A schedule that resumes at capture tick `capture` under the epoch `(delay, delay_from)`:
    /// what a seat that caught up from a state transfer stamps with. The captures before
    /// `capture` are not this seat's, so the first stamp leaves no gap of its own.
    pub(crate) fn resume_at(capture: u64, delay: u16, delay_from: u64) -> Self {
        let mut s = Self::new(delay);
        if let Some(first) = s.epochs.front_mut() {
            first.0 = delay_from;
        }
        s.next_capture = capture;
        s.last_stamp = capture.saturating_add(u64::from(delay)).checked_sub(1);
        s
    }

    /// The delay in force from capture tick `from` on. A change at or before the newest epoch's
    /// start replaces it (the same change repeated is a no-op); returns whether anything changed.
    pub(crate) fn set_delay(&mut self, from: u64, delay: u16) -> bool {
        match self.epochs.back_mut() {
            Some((start, current)) if *start >= from => {
                if *current == delay {
                    return false;
                }
                *current = delay;
            }
            Some((_, current)) if *current == delay => return false,
            _ => {
                if self.epochs.len() >= MAX_EPOCHS {
                    self.epochs.pop_front();
                }
                self.epochs.push_back((from, delay));
            }
        }
        true
    }

    /// The delay for capture tick `capture`.
    pub(crate) fn delay_at(&self, capture: u64) -> u16 {
        self.epochs
            .iter()
            .rev()
            .find(|(from, _)| *from <= capture)
            .or(self.epochs.front())
            .map_or(0, |(_, d)| *d)
    }

    /// The newest epoch: `(delay, first capture tick)`.
    pub(crate) fn current(&self) -> (u16, u64) {
        self.epochs.back().map_or((0, 0), |(from, d)| (*d, *from))
    }

    /// The next capture tick to stamp.
    pub(crate) fn next_capture(&self) -> u64 {
        self.next_capture
    }

    /// Stamps the next capture tick with `extra` ticks of own lead and moves on.
    pub(crate) fn stamp_next(&mut self, extra: u16) -> (u64, Stamp) {
        let capture = self.next_capture;
        self.next_capture += 1;
        let lead = u32::from(self.delay_at(capture)) + u32::from(extra);
        let tick = capture.saturating_add(u64::from(lead));
        let stamp = match self.last_stamp {
            Some(last) if tick <= last => Stamp::Skipped,
            last => {
                let first_free = last.map_or(0, |l| l + 1);
                if tick > first_free {
                    if self.gaps.len() >= MAX_EPOCHS {
                        self.gaps.pop_front();
                    }
                    self.gaps.push_back((first_free, tick - 1));
                    self.gap_ticks += tick - first_free;
                }
                if self.stamp_runs.back().is_none_or(|(_, l)| *l != lead) {
                    if self.stamp_runs.len() >= MAX_EPOCHS {
                        self.stamp_runs.pop_front();
                    }
                    self.stamp_runs.push_back((tick, lead));
                }
                self.last_stamp = Some(tick);
                Stamp::Tick(tick)
            }
        };
        (capture, stamp)
    }

    /// Whether no capture maps to `tick` (a gap tick of a raise or of the start). Ticks past the
    /// newest stamp are not known yet and answer `false`.
    pub(crate) fn is_gap(&self, tick: u64) -> bool {
        self.gaps.iter().any(|(a, b)| (*a..=*b).contains(&tick))
    }

    /// Gap ticks so far.
    pub(crate) fn gap_ticks(&self) -> u64 {
        self.gap_ticks
    }

    /// The lead (delay plus extra) the capture that stamped `tick` used; `None` for a tick before
    /// the first stamp.
    pub(crate) fn lead_of_stamp(&self, tick: u64) -> Option<u32> {
        self.stamp_runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= tick)
            .map(|(_, lead)| *lead)
    }

    /// Forgets gap ranges that end before `tick`.
    pub(crate) fn prune(&mut self, tick: u64) {
        while self.gaps.front().is_some_and(|(_, end)| *end < tick) {
            self.gaps.pop_front();
        }
        while self.stamp_runs.len() > 1 && self.stamp_runs.get(1).is_some_and(|(first, _)| *first <= tick) {
            self.stamp_runs.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(s: &mut Stamper, n: usize) -> Vec<Stamp> {
        (0..n).map(|_| s.stamp_next(0).1).collect()
    }

    #[test]
    fn the_start_is_a_raise_from_zero_and_its_gap_is_listed() {
        let mut s = Stamper::new(3);
        assert_eq!(ticks(&mut s, 3), [Stamp::Tick(3), Stamp::Tick(4), Stamp::Tick(5)]);
        assert!(s.is_gap(0) && s.is_gap(2) && !s.is_gap(3));
        assert_eq!(s.gap_ticks(), 3);
    }

    #[test]
    fn a_raise_leaves_exactly_the_gap_ticks_and_no_tick_twice() {
        let mut s = Stamper::new(2);
        let mut seen = ticks(&mut s, 4); // captures 0..=3 -> ticks 2..=5
        assert!(s.set_delay(4, 5));
        seen.extend(ticks(&mut s, 3)); // captures 4..=6 -> ticks 9..=11
        assert_eq!(
            seen,
            [2, 3, 4, 5, 9, 10, 11].map(Stamp::Tick),
            "capture 4 jumps to 4 + 5"
        );
        assert!((6..=8).all(|t| s.is_gap(t)), "ticks 6..=8 are the gap");
        assert!(!s.is_gap(5) && !s.is_gap(9));
        assert_eq!(s.lead_of_stamp(5), Some(2));
        assert_eq!(s.lead_of_stamp(9), Some(5));
    }

    #[test]
    fn a_lower_skips_one_capture_per_tick_lowered() {
        let mut s = Stamper::new(4);
        assert_eq!(ticks(&mut s, 2), [Stamp::Tick(4), Stamp::Tick(5)]);
        assert!(s.set_delay(2, 3));
        // Capture 2 would stamp 2 + 3 = 5 again: skipped. Capture 3 stamps 6.
        assert_eq!(ticks(&mut s, 2), [Stamp::Skipped, Stamp::Tick(6)]);
        assert!(s.set_delay(4, 1));
        // 4 + 1 = 5 and 5 + 1 = 6 are taken; capture 6 stamps 7.
        assert_eq!(ticks(&mut s, 3), [Stamp::Skipped, Stamp::Skipped, Stamp::Tick(7)]);
        assert_eq!(s.gap_ticks(), 4, "only the start gap");
    }

    #[test]
    fn a_repeated_or_same_delay_change_is_a_no_op() {
        let mut s = Stamper::new(4);
        assert!(!s.set_delay(0, 4));
        assert!(s.set_delay(10, 6));
        assert!(!s.set_delay(10, 6), "the frame repeats the epoch");
        assert!(!s.set_delay(12, 6), "same delay later");
        assert_eq!(s.delay_at(9), 4);
        assert_eq!(s.delay_at(10), 6);
        assert_eq!(s.current(), (6, 10));
    }

    #[test]
    fn an_own_lead_moves_only_that_stamper() {
        let mut a = Stamper::new(2);
        let mut b = Stamper::new(2);
        assert_eq!(a.stamp_next(0).1, Stamp::Tick(2));
        assert_eq!(b.stamp_next(0).1, Stamp::Tick(2));
        assert_eq!(a.stamp_next(2).1, Stamp::Tick(5), "1 + 2 + 2");
        assert_eq!(b.stamp_next(0).1, Stamp::Tick(3));
        assert_eq!(
            a.stamp_next(0).1,
            Stamp::Skipped,
            "2 + 2 = 4 <= 5: the lead dropped"
        );
        assert_eq!(a.lead_of_stamp(5), Some(4));
    }
}
