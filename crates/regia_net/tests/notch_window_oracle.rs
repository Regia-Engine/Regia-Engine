// SPDX-License-Identifier: MPL-2.0
//! Independent timestamp-window and notch transition contracts of `congestion::LossNotches` (the
//! loss-step budget that replaced the removed `CongestionNotches` name in v0.7, ADR-10067).

use regia_net::congestion::{LossNotches, NotchConfig};

#[derive(Clone, Copy)]
struct Point {
    at: u64,
    loss: f32,
}

struct Oracle {
    config: NotchConfig,
    floor: usize,
    ceiling: usize,
    budget: usize,
    samples: Vec<(u64, f32)>,
    last_change: Option<u64>,
}

impl Oracle {
    fn new(ceiling: usize, floor: usize, config: NotchConfig) -> Self {
        Self {
            config,
            floor,
            ceiling,
            budget: ceiling,
            samples: Vec::new(),
            last_change: None,
        }
    }

    fn observe(&mut self, point: Point) -> (f32, usize) {
        self.samples.push((point.at, point.loss.clamp(0.0, 1.0)));
        let cutoff = point.at.saturating_sub(self.config.window_ms);
        self.samples.retain(|(at, _)| *at >= cutoff);
        let average = self.samples.iter().map(|(_, loss)| *loss).sum::<f32>() / self.samples.len() as f32;
        let due = self
            .last_change
            .is_none_or(|last| point.at.saturating_sub(last) >= self.config.interval_ms);
        if due {
            if average > self.config.cut_above && self.budget > self.floor {
                self.budget = self.budget.saturating_sub(self.config.step).max(self.floor);
                self.last_change = Some(point.at);
            } else if average < self.config.recover_below && self.budget < self.ceiling {
                self.budget = self.budget.saturating_add(self.config.step).min(self.ceiling);
                self.last_change = Some(point.at);
            }
        }
        (average, self.budget)
    }
}

fn check_trace(name: &str, ceiling: usize, floor: usize, config: NotchConfig, trace: &[Point]) {
    let mut controller = LossNotches::new(ceiling, floor, config);
    let mut oracle = Oracle::new(ceiling.max(1), floor.clamp(1, ceiling.max(1)), config);
    for (index, point) in trace.iter().copied().enumerate() {
        let (expected_average, expected_budget) = oracle.observe(point);
        controller.observe(point.at, point.loss);
        assert!(
            (controller.average_loss() - expected_average).abs() < 0.000_01,
            "{name} sample {index}: average loss"
        );
        assert_eq!(
            controller.budget(),
            expected_budget,
            "{name} sample {index}: budget"
        );
    }
}

#[test]
fn window_cutoff_is_inclusive_until_the_sample_is_one_millisecond_expired() {
    let config = NotchConfig {
        step: 1,
        window_ms: 10,
        cut_above: 0.4,
        recover_below: 0.1,
        interval_ms: 1,
    };
    check_trace(
        "inclusive cutoff",
        10,
        1,
        config,
        &[
            Point { at: 100, loss: 1.0 },
            Point { at: 110, loss: 0.0 },
            Point { at: 111, loss: 0.0 },
        ],
    );
}

#[test]
fn interval_edges_threshold_equality_and_long_quiet_hysteresis_match_oracle() {
    let config = NotchConfig {
        step: 3,
        window_ms: 1_000,
        cut_above: 0.5,
        recover_below: 0.25,
        interval_ms: 10,
    };
    check_trace(
        "interval and threshold edges",
        11,
        2,
        config,
        &[
            Point { at: 0, loss: 0.5 },
            Point { at: 1, loss: 0.5 },
            Point { at: 9, loss: 1.0 },
            Point { at: 18, loss: 1.0 },
            Point { at: 19, loss: 1.0 },
            Point { at: 20, loss: 0.0 },
            Point { at: 30, loss: 0.0 },
            Point {
                at: 10_000,
                loss: 0.0,
            },
            Point {
                at: 10_009,
                loss: 1.0,
            },
            Point {
                at: 10_010,
                loss: 1.0,
            },
            Point {
                at: 10_011,
                loss: 0.0,
            },
        ],
    );
}

#[test]
fn average_equal_to_recovery_threshold_holds_until_loss_falls_below_it() {
    let config = NotchConfig {
        step: 2,
        window_ms: 10,
        cut_above: 0.5,
        recover_below: 0.25,
        interval_ms: 1,
    };
    check_trace(
        "recovery threshold equality",
        10,
        2,
        config,
        &[
            Point { at: 0, loss: 1.0 },
            Point { at: 1, loss: 0.0 },
            Point { at: 2, loss: 0.0 },
            Point { at: 3, loss: 0.0 },
            Point { at: 4, loss: 0.0 },
        ],
    );
}

#[test]
fn zero_window_zero_interval_repeated_times_and_saturated_bounds_are_defined() {
    let config = NotchConfig {
        step: usize::MAX,
        window_ms: 0,
        cut_above: 0.2,
        recover_below: 0.1,
        interval_ms: 0,
    };
    check_trace(
        "zero values and saturation",
        17,
        6,
        config,
        &[
            Point { at: 5, loss: 1.0 },
            Point { at: 5, loss: 1.0 },
            Point { at: 5, loss: 0.0 },
            Point { at: 5, loss: 0.0 },
            Point { at: 6, loss: 0.0 },
            Point { at: 7, loss: 1.0 },
        ],
    );
}

#[test]
fn near_max_timestamp_does_not_overflow_window_or_interval_arithmetic() {
    let config = NotchConfig {
        step: 2,
        window_ms: 20,
        cut_above: 0.4,
        recover_below: 0.1,
        interval_ms: 7,
    };
    let start = u64::MAX - 27;
    check_trace(
        "near max timestamp",
        9,
        3,
        config,
        &[
            Point { at: start, loss: 1.0 },
            Point {
                at: start + 6,
                loss: 0.0,
            },
            Point {
                at: start + 7,
                loss: 0.0,
            },
            Point {
                at: start + 20,
                loss: 0.0,
            },
            Point {
                at: u64::MAX,
                loss: 0.0,
            },
        ],
    );
}

#[test]
fn interleaved_controllers_keep_client_windows_and_budgets_independent() {
    let config = NotchConfig {
        step: 2,
        window_ms: 10,
        cut_above: 0.4,
        recover_below: 0.1,
        interval_ms: 5,
    };
    let mut left = LossNotches::new(10, 2, config);
    let mut right = LossNotches::new(10, 2, config);
    let mut left_ref = Oracle::new(10, 2, config);
    let mut right_ref = Oracle::new(10, 2, config);
    for (index, point) in [
        (Point { at: 0, loss: 1.0 }, Point { at: 0, loss: 0.0 }),
        (Point { at: 5, loss: 1.0 }, Point { at: 5, loss: 0.0 }),
        (Point { at: 10, loss: 0.0 }, Point { at: 10, loss: 0.0 }),
        (Point { at: 15, loss: 0.0 }, Point { at: 15, loss: 1.0 }),
        (Point { at: 30, loss: 0.0 }, Point { at: 30, loss: 1.0 }),
    ]
    .into_iter()
    .enumerate()
    {
        let (la, lb) = (left_ref.observe(point.0), right_ref.observe(point.1));
        left.observe(point.0.at, point.0.loss);
        right.observe(point.1.at, point.1.loss);
        assert!(
            (left.average_loss() - la.0).abs() < 0.000_01,
            "left average {index}"
        );
        assert!(
            (right.average_loss() - lb.0).abs() < 0.000_01,
            "right average {index}"
        );
        assert_eq!(left.budget(), la.1, "left budget {index}");
        assert_eq!(right.budget(), lb.1, "right budget {index}");
    }
}
