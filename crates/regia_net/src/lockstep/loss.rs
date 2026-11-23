// SPDX-License-Identifier: MPL-2.0
//! Loss runs of a datagram sequence and the p99 run a link's redundancy must cover.
//!
//! Each lockstep datagram carries a per-direction sequence number. The receiver sees a loss run
//! as a jump in the sequence. The run-length distribution of a bursty link follows the
//! Gilbert-Elliott model (E. N. Gilbert, "Capacity of a Burst-Noise Channel", Bell System
//! Technical Journal 39, 1960; E. O. Elliott, "Estimates of Error Rates for Codes on Burst-Noise
//! Channels", BSTJ 42, 1963): while the channel loses, each further datagram is lost with a fixed
//! probability `q`, so run lengths are geometric and `P(run > k) = q^k`. With `lost` datagrams in
//! `runs` runs the maximum-likelihood estimate is `q = (lost - runs) / lost`.
//!
//! A run of `k` datagrams is repaired by the datagram after it, `k` send intervals late, so the
//! p99 run is the time the delay budget has to hold for loss (spec 120 § 7.3). Everything here is
//! integer arithmetic: the result is the same on every host.
//!
//! A jump in the sequence is not always a wait. When a path gets faster, new datagrams overtake
//! the ones still in flight: the sequence jumps and nothing was late. So a run counts only the
//! send intervals the receiver actually went without a datagram: a jump of `g` after `n`
//! intervals of silence is a run of `min(g, n - 1)`.

use std::collections::VecDeque;

/// How long a loss run is remembered, milliseconds.
pub(crate) const LOSS_MEMORY_MS: u64 = 60_000;
/// A jump longer than this many datagrams is an outage, not a loss run: redundancy cannot cover
/// it, and it must not size the delay.
pub(crate) const MAX_RUN: u32 = 32;
/// Runs remembered at most.
const MAX_RUNS: usize = 512;

/// The smallest `k` with `P(run > k) <= 1 %` under the geometric fit, capped at [`MAX_RUN`];
/// `0` without losses.
pub(crate) fn p99_loss_run(lost: u64, runs: u64) -> u32 {
    if lost == 0 || runs == 0 {
        return 0;
    }
    if lost <= runs {
        return 1;
    }
    // q in Q32 fixed point; lost > runs >= 1, so 0 < q < 1.
    let q = (u128::from(lost - runs) << 32) / u128::from(lost);
    let one = 1u128 << 32;
    let mut acc = one;
    for k in 1..=MAX_RUN {
        acc = (acc * q) >> 32;
        if acc * 100 <= one {
            return k;
        }
    }
    MAX_RUN
}

/// The share of a direction's datagrams the delay budget may leave late for loss, parts per
/// million: half a percent each way, so about one input in a hundred is late for loss with both
/// directions lossy.
pub(crate) const LOSS_TAIL_PPM: u64 = 5_000;

/// The loss term of the delay budget (spec 120 § 7.3 as amended by ADR-8209): the smallest `k`
/// such that at most [`LOSS_TAIL_PPM`] of **all** datagrams are delayed more than `k` send
/// intervals by loss, capped at [`MAX_RUN`]; `0` without losses.
///
/// What a lost datagram carried arrives with the next datagram that gets through, one send
/// interval later per datagram of the run that is still to come. So a datagram's delay `D` is the
/// number of consecutive losses that start with it, and under the geometric fit
/// `P(D > k) = p * q^k`, with the loss rate `p = lost / sent` and `q = (lost - runs) / lost`.
/// [`p99_loss_run`] is the tail of `D` among the lost datagrams only (`q^k`): it budgets a link
/// that loses one datagram in a thousand like one that loses one in ten.
pub(crate) fn loss_delay_tail(sent: u64, lost: u64, runs: u64) -> u32 {
    if sent == 0 || lost == 0 || runs == 0 {
        return 0;
    }
    let lost = lost.min(sent);
    let runs = runs.min(lost);
    let one = 1u128 << 32;
    let limit = one * u128::from(LOSS_TAIL_PPM) / 1_000_000;
    let q = (u128::from(lost - runs) << 32) / u128::from(lost);
    // `p * q^k` in Q32, from k = 0 (the loss rate itself).
    let mut tail = (u128::from(lost) << 32) / u128::from(sent);
    for k in 0..MAX_RUN {
        if tail <= limit {
            return k;
        }
        tail = (tail * q) >> 32;
    }
    MAX_RUN
}

/// What one sequence number told the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeqClass {
    /// The next expected datagram, or the first one.
    InOrder,
    /// Newer than expected: this many datagrams before it have not arrived.
    AfterLoss(u64),
    /// At or before one already seen (a duplicate or a reordered datagram).
    Old,
}

/// Receiver-side tracker of one direction's datagram sequence.
#[derive(Debug, Clone)]
pub(crate) struct LossRuns {
    /// The sender's send interval, microseconds.
    interval_us: u64,
    /// Arrival of the datagram that last moved the sequence on.
    last_advance_ms: u64,
    /// Arrival of the first datagram.
    first_ms: Option<u64>,
    /// The newest sequence number seen. (Not "the next expected": a peer picks its numbers, and
    /// the last one of the range has no next.)
    newest: Option<u64>,
    /// `(arrival ms of the datagram that ended the run, run length)`, oldest first.
    recent: VecDeque<(u64, u32)>,
    /// `(arrival ms, sequence jump)` of every jump, whatever the timing: what a receiver that
    /// needs every datagram in order is missing when the next one arrives.
    gaps: VecDeque<(u64, u32)>,
    received: u64,
    lost: u64,
}

impl Default for LossRuns {
    fn default() -> Self {
        Self::with_interval_us(16_666)
    }
}

impl LossRuns {
    /// A tracker for a sender that sends every `interval_us` microseconds.
    pub(crate) fn with_interval_us(interval_us: u64) -> Self {
        Self {
            interval_us: interval_us.max(1),
            last_advance_ms: 0,
            first_ms: None,
            newest: None,
            recent: VecDeque::new(),
            gaps: VecDeque::new(),
            received: 0,
            lost: 0,
        }
    }

    /// Takes the sequence number of a datagram that arrived at `now_ms`.
    pub(crate) fn on_seq(&mut self, seq: u64, now_ms: u64) -> SeqClass {
        let class = match self.newest {
            None => SeqClass::InOrder,
            Some(newest) if seq <= newest => return SeqClass::Old,
            Some(newest) if seq - newest == 1 => SeqClass::InOrder,
            Some(newest) => {
                let gap = seq - newest - 1;
                self.lost = self.lost.saturating_add(gap);
                if gap <= u64::from(MAX_RUN) {
                    if self.gaps.len() >= MAX_RUNS {
                        self.gaps.pop_front();
                    }
                    self.gaps.push_back((now_ms, gap as u32));
                }
                // Whole send intervals the receiver went without a datagram, beyond the one that
                // always passes: a jump with no silence is overtaking, not loss.
                let silent_us = now_ms.saturating_sub(self.last_advance_ms).saturating_mul(1000);
                let waited =
                    (silent_us.saturating_add(self.interval_us / 2) / self.interval_us).saturating_sub(1);
                let run = gap.min(waited);
                if (1..=u64::from(MAX_RUN)).contains(&run) {
                    if self.recent.len() >= MAX_RUNS {
                        self.recent.pop_front();
                    }
                    self.recent.push_back((now_ms, run as u32));
                }
                SeqClass::AfterLoss(gap)
            }
        };
        self.received = self.received.saturating_add(1);
        self.first_ms.get_or_insert(now_ms);
        self.newest = Some(seq);
        self.last_advance_ms = now_ms;
        class
    }

    /// Datagrams received (duplicates and reordered ones excluded).
    pub(crate) fn received(&self) -> u64 {
        self.received
    }

    /// Datagrams the sequence shows lost so far.
    pub(crate) fn lost(&self) -> u64 {
        self.lost
    }

    /// The newest sequence number seen.
    pub(crate) fn newest(&self) -> Option<u64> {
        self.newest
    }

    /// The run the input delay budgets for, over the last [`LOSS_MEMORY_MS`]: the loss delay
    /// that all but [`LOSS_TAIL_PPM`] of the direction's datagrams stay within
    /// ([`loss_delay_tail`]), never above the longest run actually seen (a single long run must
    /// not be read as a fat tail). The datagrams sent in the window are counted from the sender's
    /// interval: it sends one per interval whether they arrive or not.
    pub(crate) fn p99_run(&mut self, now_ms: u64) -> u32 {
        let horizon = now_ms.saturating_sub(LOSS_MEMORY_MS);
        while self.recent.front().is_some_and(|(at, _)| *at < horizon) {
            self.recent.pop_front();
        }
        let runs = self.recent.len() as u64;
        let lost: u64 = self.recent.iter().map(|(_, len)| u64::from(*len)).sum();
        let longest = self.recent.iter().map(|(_, len)| *len).max().unwrap_or(0);
        let since = self.first_ms.map_or(now_ms, |first| first.max(horizon));
        let sent = now_ms.saturating_sub(since).saturating_mul(1000) / self.interval_us;
        loss_delay_tail(sent.max(lost), lost, runs).min(longest)
    }

    /// The sequence jump a redundancy window has to cover: the fitted p99 jump over the last
    /// [`LOSS_MEMORY_MS`] and at least the longest one seen in it. Jumps count whatever their
    /// timing (a datagram that was overtaken is as missing as a lost one when the next arrives),
    /// and a window that is too long only costs bytes.
    pub(crate) fn window_run(&mut self, now_ms: u64) -> u32 {
        let horizon = now_ms.saturating_sub(LOSS_MEMORY_MS);
        while self.gaps.front().is_some_and(|(at, _)| *at < horizon) {
            self.gaps.pop_front();
        }
        let jumps = self.gaps.len() as u64;
        let lost: u64 = self.gaps.iter().map(|(_, len)| u64::from(*len)).sum();
        let longest = self.gaps.iter().map(|(_, len)| *len).max().unwrap_or(0);
        p99_loss_run(lost, jumps).max(longest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_losses_need_one_tick_and_bursts_follow_the_geometric_tail() {
        assert_eq!(p99_loss_run(0, 0), 0);
        assert_eq!(p99_loss_run(40, 40), 1, "every run is one datagram");
        // q = 0.5: 0.5^7 = 0.78 % <= 1 % < 0.5^6.
        assert_eq!(p99_loss_run(200, 100), 7);
        // The bad-wifi profile's bursts continue with probability about 0.43: 0.43^6 = 0.63 %.
        assert_eq!(p99_loss_run(1000, 570), 6);
        // A link that almost never recovers is capped.
        assert_eq!(p99_loss_run(1_000_000, 1), MAX_RUN);
    }

    #[test]
    fn the_delay_tail_weighs_the_runs_by_how_often_the_link_loses() {
        assert_eq!(loss_delay_tail(0, 0, 0), 0);
        assert_eq!(loss_delay_tail(3_600, 0, 0), 0);
        // One datagram in a thousand lost: fewer than the tail the budget leaves late.
        assert_eq!(loss_delay_tail(10_000, 10, 10), 0);
        // 0.5 % lost exactly, single losses: still inside the tail.
        assert_eq!(loss_delay_tail(10_000, 50, 50), 0);
        // 1 % lost, single losses: one interval covers every one of them.
        assert_eq!(loss_delay_tail(10_000, 100, 100), 1);
        // The bad-wifi profile: 3 % lost in runs that continue with probability 0.43.
        // 0.03, 0.0129, 0.0055, 0.0024: three intervals leave 0.24 % late.
        assert_eq!(loss_delay_tail(10_000, 300, 171), 3);
        assert_eq!(
            p99_loss_run(300, 171),
            6,
            "the run tail among lost datagrams alone"
        );
        // The same runs on a link that loses ten times less often: 0.3 % is inside the tail.
        assert_eq!(loss_delay_tail(100_000, 300, 171), 0);
        // Everything lost in one run: capped.
        assert_eq!(loss_delay_tail(1_000, 1_000, 1), MAX_RUN);
        // More lost than sent cannot be: the rate is held at one.
        assert_eq!(loss_delay_tail(10, 40, 40), 1);
    }

    #[test]
    fn the_tracker_counts_runs_and_ignores_old_and_outage_jumps() {
        let mut t = LossRuns::default();
        assert_eq!(t.on_seq(0, 0), SeqClass::InOrder);
        assert_eq!(t.on_seq(1, 16), SeqClass::InOrder);
        assert_eq!(t.on_seq(4, 64), SeqClass::AfterLoss(2));
        assert_eq!(t.on_seq(3, 70), SeqClass::Old, "reordered: already counted lost");
        assert_eq!(t.on_seq(4, 71), SeqClass::Old, "duplicate");
        assert_eq!(t.on_seq(5, 80), SeqClass::InOrder);
        assert_eq!((t.received(), t.lost(), t.newest()), (4, 2, Some(5)));
        assert_eq!(
            t.p99_run(100),
            2,
            "one run of two among six datagrams: the tail is capped by the longest run"
        );
        assert_eq!(
            t.window_run(100),
            7,
            "the window takes the fit: half of the lost ones continued a run"
        );
        // A 500-datagram jump is an outage: counted lost, not a run. Over nine seconds the two
        // datagrams of the one run are 0.37 % of what was sent: under the tail the budget
        // leaves late, so the delay holds nothing for them.
        assert_eq!(t.on_seq(506, 9_000), SeqClass::AfterLoss(500));
        assert_eq!(t.p99_run(9_000), 0);
        // The run ages out.
        assert_eq!(t.p99_run(64 + LOSS_MEMORY_MS + 1), 0);
    }

    #[test]
    fn a_jump_without_silence_is_overtaking_and_sizes_nothing() {
        // The path got 55 ms faster: datagram 8 arrives one interval after datagram 3, ahead of
        // 4..=7, which follow as old. Nothing was late, so nothing is a loss run.
        let mut t = LossRuns::default();
        for (seq, at) in [(0u64, 0u64), (1, 17), (2, 33), (3, 50)] {
            assert_eq!(t.on_seq(seq, at), SeqClass::InOrder);
        }
        assert_eq!(t.on_seq(8, 67), SeqClass::AfterLoss(4));
        for (seq, at) in [(4u64, 70u64), (5, 87), (6, 103), (7, 120)] {
            assert_eq!(t.on_seq(seq, at), SeqClass::Old);
        }
        assert_eq!(t.p99_run(200), 0);
        assert!(
            t.window_run(200) >= 4,
            "the window still has to cover the four that were overtaken"
        );
        // A held datagram: 10 arrives two intervals after 8, 9 is still out. One interval late.
        assert_eq!(t.on_seq(10, 100), SeqClass::AfterLoss(1));
        assert_eq!(t.p99_run(200), 1);
        // Jitter squeezed the gap under half an interval of extra silence: nothing waited.
        assert_eq!(t.on_seq(12, 120), SeqClass::AfterLoss(1));
        assert_eq!(t.p99_run(200), 1, "still the one run");
    }

    /// A peer picks its sequence numbers: the last one of the range must not overflow the
    /// tracker (it used to keep `newest + 1`).
    #[test]
    fn the_last_sequence_number_of_the_range_is_tracked_without_overflow() {
        let mut t = LossRuns::with_interval_us(16_667);
        assert_eq!(t.on_seq(u64::MAX, 10), SeqClass::InOrder);
        assert_eq!(t.newest(), Some(u64::MAX));
        assert_eq!(t.on_seq(5, 20), SeqClass::Old);
        assert_eq!(t.on_seq(u64::MAX, 30), SeqClass::Old, "a repeat");
        let mut t = LossRuns::with_interval_us(16_667);
        assert_eq!(t.on_seq(0, 10), SeqClass::InOrder);
        assert_eq!(
            t.on_seq(u64::MAX, 27),
            SeqClass::AfterLoss(u64::MAX - 1),
            "a jump over the whole range"
        );
        assert_eq!(t.newest(), Some(u64::MAX));
    }
}
