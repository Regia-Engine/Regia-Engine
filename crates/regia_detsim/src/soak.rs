// SPDX-License-Identifier: MPL-2.0
//! Constant-memory repeated comparisons of seeded F-02 scenarios (ADR-8151).
//!
//! Host factories create independent state. Both bootstraps and every subsequent
//! tick are compared, so a transient divergence cannot hide behind a final match.
//! The runner stores two live scenarios and counters, never a tick-length trace.

use crate::golden::{DetRun, DetScenario};
use crate::harness::{HarnessError, SimHarness, SimSnapshot};

/// A mismatch at a tick boundary in one pair of independent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakDivergence {
    /// Zero-based index of the failed pair.
    pub pair: u32,
    /// Expected state; tick zero means bootstrap, tick one means after step(0).
    pub expected: SimSnapshot,
    /// Actual state at the same boundary.
    pub actual: SimSnapshot,
}

/// Results of a bounded soak; timing never enters this report or its hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakReport {
    /// Fully matching pairs (the failing pair, if any, is not counted).
    pub pairs_completed: u32,
    /// Tick boundaries compared, excluding bootstraps and including a mismatch.
    pub steps_compared: u64,
    /// Last expected snapshot, also meaningful for a zero-tick run.
    pub final_snapshot: SimSnapshot,
    /// First mismatch, or `None` when every pair matches.
    pub divergence: Option<SoakDivergence>,
}

impl SoakReport {
    /// Whether every requested pair matched.
    pub const fn passed(&self) -> bool {
        self.divergence.is_none()
    }
}

/// Run `pairs` independent paired bootstraps. Each factory call must construct a
/// fresh instance of the same scenario and settings, without shared mutable state.
/// Returns immediately at the first mismatch. No wall-clock stop or sleeps.
pub fn run<S: DetScenario>(
    mut factory: impl FnMut() -> S,
    run: DetRun,
    pairs: u32,
) -> Result<SoakReport, HarnessError> {
    if pairs == 0 {
        return Err(HarnessError::EmptySoak);
    }
    let _span = tracing::info_span!("detsim_soak", pairs, ticks = run.ticks).entered();
    let mut report = SoakReport {
        pairs_completed: 0,
        steps_compared: 0,
        final_snapshot: SimSnapshot {
            tick: 0,
            fingerprint: 0,
        },
        divergence: None,
    };
    for pair in 0..pairs {
        let mut left = SimHarness::new(factory(), run)?;
        let mut right = SimHarness::new(factory(), run)?;
        loop {
            let expected = left.snapshot();
            let actual = right.snapshot();
            report.final_snapshot = expected;
            if expected != actual {
                report.divergence = Some(SoakDivergence {
                    pair,
                    expected,
                    actual,
                });
                return Ok(report);
            }
            if left.remaining() == 0 {
                break;
            }
            left.step()?;
            right.step()?;
            report.steps_compared += 1;
        }
        report.pairs_completed += 1;
    }
    Ok(report)
}
