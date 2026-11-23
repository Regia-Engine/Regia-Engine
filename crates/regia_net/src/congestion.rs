// SPDX-License-Identifier: MPL-2.0
//! Loss-driven send budget ("notches").
//!
//! This is not congestion control: the transport's rate controller paces every connection
//! (spec 120 § 4.2), and a game sizes what it sends per tick by the transport's allowance
//! ([`crate::connection::Connection::send_allowance_bytes_per_second`],
//! [`crate::NetServer::send_allowance`]). [`LossNotches`] is a helper for a game that also
//! wants to trade content for loss (fewer cosmetic entities, coarser effects) on top of that
//! allowance. Until v0.7 it was named `CongestionNotches` (deprecated in v0.6, removed in v0.7,
//! spec 120 § 12.3, ADR-10067); the behavior is unchanged.
//!
//! A per-client budget (entities, bytes or messages per send; the game decides the unit) starts at
//! its ceiling and moves one notch at most once per interval: down while the average loss over the
//! window is above the cut threshold, up while it is below the recover threshold. Send rate never
//! changes here, only how much each send carries. This is additive-step congestion avoidance in
//! the spirit of G. Fiedler, "Reliability and Congestion Avoidance over UDP" (gafferongames.com,
//! 2008), with symmetric steps so recovery is as fast as the cut.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Notch controller tuning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotchConfig {
    /// Budget moved per notch.
    pub step: usize,
    /// Window the loss average covers, in milliseconds.
    pub window_ms: u64,
    /// Average loss above which the budget steps down.
    pub cut_above: f32,
    /// Average loss below which the budget steps up.
    pub recover_below: f32,
    /// Minimum time between notches, in milliseconds.
    pub interval_ms: u64,
}

impl Default for NotchConfig {
    fn default() -> Self {
        Self {
            step: 4,
            window_ms: 2_000,
            cut_above: 0.05,
            recover_below: 0.02,
            interval_ms: 1_000,
        }
    }
}

/// Per-client loss-step budget (named `CongestionNotches` until v0.7, ADR-10067).
///
/// It steps on loss alone, so random wireless loss lowers it on a link with spare bandwidth
/// (spec 120 § 4.2, F7): never size a send by it alone. Size sends by the transport's allowance
/// ([`crate::connection::Connection::send_allowance_bytes_per_second`],
/// [`crate::NetServer::send_allowance`]) and use this budget, if at all, as a cap below that.
#[derive(Debug, Clone)]
pub struct LossNotches {
    config: NotchConfig,
    budget: usize,
    max: usize,
    min: usize,
    samples: VecDeque<(u64, f32)>,
    last_notch_ms: Option<u64>,
}

impl LossNotches {
    /// A controller between `min` and `max`, starting at `max` (no slow start).
    ///
    /// The config is sanitized (it may come from a settings file): a step of 0 acts as 1, and a
    /// threshold that is NaN or outside `0..=1` falls back to the default (`cut_above` 0.05,
    /// `recover_below` 0.02) or is clamped into range. The controller is a helper for the game's
    /// send path; the engine's own replication does not drive it.
    pub fn new(max: usize, min: usize, config: NotchConfig) -> Self {
        let max = max.max(1);
        let min = min.clamp(1, max);
        let defaults = NotchConfig::default();
        let threshold = |v: f32, fallback: f32| if v.is_nan() { fallback } else { v.clamp(0.0, 1.0) };
        let config = NotchConfig {
            step: config.step.max(1),
            cut_above: threshold(config.cut_above, defaults.cut_above),
            recover_below: threshold(config.recover_below, defaults.recover_below),
            ..config
        };
        Self {
            config,
            budget: max,
            max,
            min,
            samples: VecDeque::new(),
            last_notch_ms: None,
        }
    }

    /// The current budget.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// The ceiling.
    pub fn max(&self) -> usize {
        self.max
    }

    /// The floor.
    pub fn min(&self) -> usize {
        self.min
    }

    /// Records a loss observation (`0..=1`) at `now_ms` and steps the budget when a notch is due.
    pub fn observe(&mut self, now_ms: u64, loss: f32) {
        let loss = if loss.is_nan() { 0.0 } else { loss.clamp(0.0, 1.0) };
        self.samples.push_back((now_ms, loss));
        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
        if self
            .last_notch_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < self.config.interval_ms)
        {
            return;
        }
        let avg = self.average_loss();
        if avg > self.config.cut_above && self.budget > self.min {
            self.budget = self.budget.saturating_sub(self.config.step).max(self.min);
            self.last_notch_ms = Some(now_ms);
        } else if avg < self.config.recover_below && self.budget < self.max {
            self.budget = self.budget.saturating_add(self.config.step).min(self.max);
            self.last_notch_ms = Some(now_ms);
        }
    }

    /// Average loss over the window (0 with no samples).
    pub fn average_loss(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|(_, l)| l).sum::<f32>() / self.samples.len() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit v034 net L5: a config read from a settings file cannot overflow the budget or switch
    /// the loss response off.
    #[test]
    fn extreme_configs_still_cut_and_recover() {
        let huge = NotchConfig {
            step: usize::MAX,
            ..NotchConfig::default()
        };
        let mut n = LossNotches::new(24, 12, huge);
        n.observe(0, 1.0);
        assert_eq!(n.budget(), 12, "a huge step cuts to the floor");
        n.observe(5_000, 0.0);
        assert_eq!(n.budget(), 24, "and recovers to the ceiling without wrapping");

        let nan = NotchConfig {
            cut_above: f32::NAN,
            recover_below: f32::NAN,
            ..NotchConfig::default()
        };
        let mut n = LossNotches::new(24, 12, nan);
        n.observe(0, 1.0);
        assert!(n.budget() < 24, "a NaN threshold still cuts on total loss");
    }

    #[test]
    fn sustained_loss_cuts_to_floor_then_recovers() {
        let mut n = LossNotches::new(24, 12, NotchConfig::default());
        assert_eq!(n.budget(), 24, "starts at the ceiling");
        let mut floor_at = None;
        for t in (0..10_000u64).step_by(100) {
            n.observe(t, 0.10);
            if floor_at.is_none() && n.budget() == 12 {
                floor_at = Some(t);
            }
        }
        assert!(
            floor_at.is_some_and(|t| t <= 3_000),
            "floor within 3 s: {floor_at:?}"
        );
        let mut ceiling_at = None;
        for t in (10_000..20_000u64).step_by(100) {
            n.observe(t, 0.0);
            if ceiling_at.is_none() && n.budget() == 24 {
                ceiling_at = Some(t);
            }
        }
        assert!(
            ceiling_at.is_some_and(|t| t <= 15_000),
            "ceiling within 5 s of clear: {ceiling_at:?}"
        );
    }

    #[test]
    fn moderate_loss_holds_and_bounds_are_sane() {
        let n = LossNotches::new(20, 50, NotchConfig::default());
        assert_eq!((n.min(), n.max()), (20, 20), "floor clamps to the ceiling");
        let mut n = LossNotches::new(40, 8, NotchConfig::default());
        for t in (0..5_000u64).step_by(100) {
            n.observe(t, 0.03);
        }
        assert_eq!(n.budget(), 40, "loss between the thresholds moves nothing");
        n.observe(5_000, f32::NAN);
        assert!(n.average_loss().is_finite());
    }
}
