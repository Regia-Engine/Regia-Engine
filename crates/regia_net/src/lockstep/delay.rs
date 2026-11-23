// SPDX-License-Identifier: MPL-2.0
//! Input delay control (spec 120 § 7.3).
//!
//! The rule Direct Order 27 runs is kept: **raise at once**, **lower by one tick after five calm
//! two-second windows**, and the change starts at a capture tick the frames carry. What sizes the
//! delay is new: not the round-trip time but what the host measures,
//!
//! ```text
//! delay = ceil(p99 input lateness / tick) + p99 loss run
//! ```
//!
//! * **Input lateness** of an input stamped for tick `T` with lead `d`: its first arrival minus
//!   the time frame `T - d` (its capture tick) is built. It is what the delay has to cover:
//!   the frame's trip down, the seat's own frame time and the input's trip up, jitter included.
//!   Only an input that arrived in the first datagram that could carry it is a sample; one that
//!   came from a redundant copy was delayed by loss, which the second term budgets.
//! * **Loss run**: the p99 run of lost datagrams on the link (the larger direction), each run
//!   repaired one send interval per datagram later (`loss.rs`).
//!
//! All integer arithmetic, in microseconds and ticks.

use std::collections::VecDeque;

use super::DelayPolicy;
use super::loss::LossRuns;

/// Lateness samples one seat's window holds at most.
const MAX_SAMPLES: usize = 1024;

/// One seat's input timing at the host.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeatTiming {
    /// `(arrival ms, lateness us)`, oldest first.
    samples: VecDeque<(u64, i64)>,
    scratch: Vec<i64>,
    /// The seat's inputs datagram sequence, as the host receives it.
    pub(crate) up: LossRuns,
    /// The p99 loss run the seat reports for the frames datagrams it receives.
    pub(crate) down_p99_run: u32,
    last_arrival_ms: Option<u64>,
}

impl SeatTiming {
    /// The timing of a seat that sends one inputs datagram every `tick_us` microseconds.
    pub(crate) fn new(tick_us: u64) -> Self {
        Self {
            up: LossRuns::with_interval_us(tick_us),
            ..Self::default()
        }
    }

    /// One clean first arrival, `lateness_us` after its capture tick's frame time.
    pub(crate) fn record(&mut self, now_ms: u64, lateness_us: i64) {
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now_ms, lateness_us));
    }

    /// Any datagram of the seat arrived.
    pub(crate) fn heard(&mut self, now_ms: u64) {
        self.last_arrival_ms = Some(now_ms);
    }

    /// Whether the seat was heard within the last window: only a live seat sizes the delay.
    pub(crate) fn live(&self, now_ms: u64, window_ms: u64) -> bool {
        self.last_arrival_ms
            .is_some_and(|at| now_ms.saturating_sub(at) <= window_ms)
    }

    /// The p99 lateness over the last `window_ms`; `None` without samples.
    pub(crate) fn lateness_p99(&mut self, now_ms: u64, window_ms: u64) -> Option<i64> {
        let horizon = now_ms.saturating_sub(window_ms);
        while self.samples.front().is_some_and(|(at, _)| *at < horizon) {
            self.samples.pop_front();
        }
        self.scratch.clear();
        self.scratch.extend(self.samples.iter().map(|(_, l)| *l));
        if self.scratch.is_empty() {
            return None;
        }
        self.scratch.sort_unstable();
        let n = self.scratch.len();
        let index = (n * 99).div_ceil(100).saturating_sub(1).min(n - 1);
        self.scratch.get(index).copied()
    }

    /// The p99 loss run of the link: the larger of the two directions.
    pub(crate) fn loss_run_p99(&mut self, now_ms: u64) -> u32 {
        self.up.p99_run(now_ms).max(self.down_p99_run)
    }

    /// The loss term of this seat's delay need under `policy` (ADR-8656). Under
    /// [`DelayPolicy::Shared`] the largest need becomes every seat's delay, so only what the host
    /// measured sizes it: the uplink run of the seat's inputs datagrams. The downlink run the seat
    /// reports is not a measurement, and one seat must not raise every seat's delay with a number
    /// it chose; the downlink still shows in the measured lateness (a frame that arrives late opens
    /// its capture late) and in that seat's own frame redundancy. Under [`DelayPolicy::PerSeat`]
    /// the frame delay follows the lower median need, which one seat cannot move, so the report
    /// counts there.
    pub(crate) fn loss_term(&mut self, now_ms: u64, policy: DelayPolicy) -> u32 {
        match policy {
            DelayPolicy::Shared => self.up.p99_run(now_ms),
            DelayPolicy::PerSeat => self.loss_run_p99(now_ms),
        }
    }

    /// The delay this seat alone needs under `policy`, ticks; `None` when it is not live or has no
    /// samples.
    pub(crate) fn need(
        &mut self,
        now_ms: u64,
        window_ms: u64,
        tick_us: u64,
        policy: DelayPolicy,
    ) -> Option<u16> {
        if !self.live(now_ms, window_ms) {
            return None;
        }
        let lateness = self.lateness_p99(now_ms, window_ms)?;
        Some(need_ticks(lateness, self.loss_term(now_ms, policy), tick_us))
    }
}

/// `ceil(lateness / tick) + loss run`, saturating.
pub(crate) fn need_ticks(lateness_us: i64, loss_run: u32, tick_us: u64) -> u16 {
    let late = u64::try_from(lateness_us).unwrap_or(0).div_ceil(tick_us.max(1));
    u16::try_from(late.saturating_add(u64::from(loss_run))).unwrap_or(u16::MAX)
}

/// The delay decision state.
#[derive(Debug, Clone)]
pub(crate) struct DelayControl {
    policy: DelayPolicy,
    window_ms: u64,
    calm_to_lower: u32,
    min: u16,
    max: u16,
    window_start_ms: Option<u64>,
    disturbed: bool,
    pub(crate) calm_windows: u32,
    pub(crate) raises: u64,
    pub(crate) lowers: u64,
    pub(crate) last_target: Option<u16>,
}

impl DelayControl {
    pub(crate) fn new(policy: DelayPolicy, window_ms: u64, calm_to_lower: u32, min: u16, max: u16) -> Self {
        Self {
            policy,
            window_ms,
            calm_to_lower,
            min,
            max,
            window_start_ms: None,
            disturbed: false,
            calm_windows: 0,
            raises: 0,
            lowers: 0,
            last_target: None,
        }
    }

    /// A live seat's input was late or missing: the current window is not calm.
    pub(crate) fn note_disturbance(&mut self) {
        self.disturbed = true;
    }

    /// The delay the needs ask for: the largest ([`DelayPolicy::Shared`]) or the lower median
    /// ([`DelayPolicy::PerSeat`]), inside the configured range; `None` without needs.
    pub(crate) fn target(&self, needs: &mut [u16]) -> Option<u16> {
        needs.sort_unstable();
        let pick = match self.policy {
            DelayPolicy::Shared => needs.last(),
            DelayPolicy::PerSeat => needs.get(needs.len().saturating_sub(1) / 2),
        }?;
        Some((*pick).clamp(self.min, self.max))
    }

    /// Decides at `now_ms` with the live seats' `needs`. Returns the new delay when it changes:
    /// a raise to the target at once, or a lower by one tick after enough calm windows in a row.
    pub(crate) fn decide(&mut self, now_ms: u64, current: u16, needs: &mut [u16]) -> Option<u16> {
        let start = *self.window_start_ms.get_or_insert(now_ms);
        let target = self.target(needs);
        self.last_target = target;
        let window_over = now_ms.saturating_sub(start) >= self.window_ms;
        let Some(target) = target else {
            // Nothing measured: the window says nothing either way.
            if window_over {
                self.window_start_ms = Some(now_ms);
                self.disturbed = false;
                self.calm_windows = 0;
            }
            return None;
        };
        if target > current {
            self.raises += 1;
            self.calm_windows = 0;
            self.disturbed = false;
            self.window_start_ms = Some(now_ms);
            return Some(target);
        }
        if !window_over {
            return None;
        }
        let calm = !self.disturbed && target < current;
        self.calm_windows = if calm { self.calm_windows + 1 } else { 0 };
        self.disturbed = false;
        self.window_start_ms = Some(now_ms);
        if self.calm_windows >= self.calm_to_lower && current > self.min {
            self.calm_windows = 0;
            self.lowers += 1;
            return Some(current - 1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_US: u64 = 16_666;

    #[test]
    fn the_need_is_lateness_in_whole_ticks_plus_the_loss_run() {
        assert_eq!(need_ticks(0, 0, TICK_US), 0);
        assert_eq!(need_ticks(-5_000, 0, TICK_US), 0, "an early input needs nothing");
        assert_eq!(need_ticks(1, 0, TICK_US), 1);
        assert_eq!(need_ticks(16_666, 0, TICK_US), 1);
        assert_eq!(need_ticks(16_667, 0, TICK_US), 2);
        assert_eq!(
            need_ticks(73_000, 6, TICK_US),
            5 + 6,
            "bad-wifi: 73 ms p99 plus a 6-datagram run"
        );
        assert_eq!(need_ticks(i64::MAX, u32::MAX, TICK_US), u16::MAX);
    }

    #[test]
    fn the_p99_is_the_window_tail_and_old_samples_leave() {
        let mut t = SeatTiming::default();
        for i in 0..100u64 {
            t.record(i * 10, i as i64 * 1_000);
        }
        // 100 samples 0..=99 ms: the p99 is the 99th of 100 (index 98).
        assert_eq!(t.lateness_p99(1_000, 2_000), Some(98_000));
        // At 2.5 s only samples from 500 ms on remain (50..=99): index ceil(50 * 0.99) - 1 = 49.
        assert_eq!(t.lateness_p99(2_500, 2_000), Some(99_000));
        assert_eq!(t.lateness_p99(10_000, 2_000), None);
    }

    #[test]
    fn a_silent_seat_does_not_size_the_delay() {
        let mut t = SeatTiming::default();
        t.record(0, 200_000);
        t.heard(0);
        assert_eq!(t.need(100, 2_000, TICK_US, DelayPolicy::Shared), Some(13));
        assert_eq!(
            t.need(2_500, 2_000, TICK_US, DelayPolicy::Shared),
            None,
            "not heard for a window"
        );
    }

    /// ADR-8656: under `Shared` the seat's reported downlink run does not size its need (one seat
    /// would raise every seat's delay with it); the measured uplink run does. Under `PerSeat` the
    /// report counts, because the lower median need cannot follow one seat.
    #[test]
    fn a_reported_downlink_run_sizes_the_need_only_under_per_seat() {
        let mut t = SeatTiming::new(TICK_US);
        t.record(0, 50_000);
        t.heard(0);
        t.down_p99_run = 32;
        assert_eq!(t.need(100, 2_000, TICK_US, DelayPolicy::Shared), Some(4));
        assert_eq!(t.loss_term(100, DelayPolicy::Shared), 0);
        assert_eq!(t.need(100, 2_000, TICK_US, DelayPolicy::PerSeat), Some(36));
        assert_eq!(t.loss_run_p99(100), 32, "the status row still shows the report");
    }

    fn control(policy: DelayPolicy) -> DelayControl {
        DelayControl::new(policy, 2_000, 5, 1, 15)
    }

    #[test]
    fn a_raise_is_at_once_and_to_the_target() {
        let mut c = control(DelayPolicy::Shared);
        assert_eq!(
            c.decide(0, 4, &mut [3, 9, 2]),
            Some(9),
            "the seat that needs the most"
        );
        assert_eq!(c.raises, 1);
        assert_eq!(c.decide(10, 9, &mut [40]), Some(15), "bounded by the maximum");
        assert_eq!(c.decide(20, 15, &mut [40]), None, "already there");
    }

    #[test]
    fn a_lower_takes_five_calm_windows_and_is_one_tick() {
        let mut c = control(DelayPolicy::Shared);
        let mut delay = 6u16;
        let mut changes = Vec::new();
        for now in (0..=30_000u64).step_by(500) {
            if let Some(d) = c.decide(now, delay, &mut [2]) {
                changes.push((now, d));
                delay = d;
            }
        }
        // Windows end at 2, 4, 6, 8 and 10 s: the fifth lowers by one; five more for the next.
        assert_eq!(changes, [(10_000, 5), (20_000, 4), (30_000, 3)]);
        assert_eq!(c.lowers, 3);
    }

    #[test]
    fn a_disturbed_window_or_a_need_at_the_delay_restarts_the_count() {
        let mut c = control(DelayPolicy::Shared);
        for now in (0..=8_000u64).step_by(500) {
            assert_eq!(c.decide(now, 6, &mut [2]), None);
        }
        assert_eq!(c.calm_windows, 4);
        c.note_disturbance();
        assert_eq!(
            c.decide(10_000, 6, &mut [2]),
            None,
            "the fifth window had a late input"
        );
        assert_eq!(c.calm_windows, 0);
        for now in (10_500..=18_000u64).step_by(500) {
            assert_eq!(c.decide(now, 6, &mut [2]), None);
        }
        assert_eq!(c.calm_windows, 4);
        assert_eq!(
            c.decide(20_000, 6, &mut [6]),
            None,
            "the need equals the delay: not calm"
        );
        assert_eq!(c.calm_windows, 0);
    }

    #[test]
    fn the_delay_never_goes_below_the_minimum_and_no_needs_changes_nothing() {
        let mut c = control(DelayPolicy::Shared);
        for now in (0..=60_000u64).step_by(1_000) {
            assert_eq!(c.decide(now, 1, &mut [0]), None);
        }
        assert_eq!(c.lowers, 0);
        let mut c = control(DelayPolicy::Shared);
        for now in (0..=60_000u64).step_by(1_000) {
            assert_eq!(c.decide(now, 7, &mut []), None);
        }
        assert_eq!((c.raises, c.lowers, c.calm_windows), (0, 0, 0));
    }

    #[test]
    fn per_seat_follows_the_median_seat() {
        let mut c = control(DelayPolicy::PerSeat);
        assert_eq!(
            c.target(&mut [2, 12, 3, 4]),
            Some(3),
            "lower median of 2, 3, 4, 12"
        );
        assert_eq!(c.target(&mut [5]), Some(5));
        assert_eq!(c.decide(0, 2, &mut [2, 12, 3, 4]), Some(3));
        let shared = control(DelayPolicy::Shared);
        assert_eq!(shared.target(&mut [2, 12, 3, 4]), Some(12));
    }
}
