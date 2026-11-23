// SPDX-License-Identifier: MPL-2.0
//! Timing observations of scale-soak passes, outside the deterministic state.
//!
//! Budgets are diagnostics, never wall-clock assertions. Quantiles use nearest
//! rank: sorted sample `ceil(percent * n / 100) - 1`. Every sample strictly over
//! budget is retained by tick, including outliers a p99 alone would hide.
//!
//! Pass times come from [`crate::profile::TickProfiler`] over the same five names
//! as [`super::SoakPass`]. The JSON object is unchanged except for an added `clock`
//! field (`"thread_cpu"` or `"wall"`).

use super::{ScaleScenario, SoakConfig, SoakObserver, SoakPass};
use crate::golden::{DetRun, DetScenario, DetTrace, current_target};
use crate::hash::Systems;
use crate::profile::{ClockKind, HostClock, ProfileSettings, TickProfiler};

/// Default diagnostic threshold: 0.1 ms, not a correctness gate.
pub const DEFAULT_BUDGET_NS: u64 = 100_000;
/// Prefix of a machine-readable timing report.
pub const REPORT_PREFIX: &str = "regia-scale-soak ";

const PASSES: Systems<5> = Systems::new(["input", "movement", "recycling", "contacts", "step"]);

/// Deterministic quantiles and exceedances from supplied nanosecond samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassSummary {
    /// Number of measured ticks (warmup excluded).
    pub samples: usize,
    /// Nearest-rank median in nanoseconds.
    pub p50_ns: u64,
    /// Nearest-rank 99th percentile in nanoseconds.
    pub p99_ns: u64,
    /// Slowest sample in nanoseconds.
    pub max_ns: u64,
    /// All zero-based sample indices strictly over budget, in observation order.
    pub over_budget: Vec<usize>,
}
impl PassSummary {
    /// Summarize samples; `None` for an empty slice. No clock reads or timing assertions.
    pub fn from_samples(samples: &[u64], budget_ns: u64) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        // n - floor((100-p)*n/100) avoids overflowing p*n for large slices.
        let rank = |p: usize| {
            let omitted = (sorted.len() / 100) * (100 - p) + ((sorted.len() % 100) * (100 - p)) / 100;
            sorted.len() - omitted - 1
        };
        Some(Self {
            samples: samples.len(),
            p50_ns: sorted[rank(50)],
            p99_ns: sorted[rank(99)],
            max_ns: sorted[sorted.len() - 1],
            over_budget: samples
                .iter()
                .enumerate()
                .filter_map(|(i, &ns)| (ns > budget_ns).then_some(i))
                .collect(),
        })
    }
}

/// Timing observer. Call `take` after one complete `step_observed` call.
/// For production tracing, implement [`SoakObserver`] with host-owned span guards instead.
pub struct PassTimer {
    profiler: Option<TickProfiler<5, HostClock>>,
    last: [u64; 5],
    taken: bool,
    ticks: u64,
}

impl Default for PassTimer {
    fn default() -> Self {
        let settings = ProfileSettings {
            ring_ticks: 1,
            tick_budget_ns: u64::MAX,
            system_budget_ns: u64::MAX,
        };
        Self {
            profiler: TickProfiler::new(PASSES, HostClock::probe(), settings).ok(),
            last: [0; 5],
            taken: true,
            ticks: 0,
        }
    }
}

impl PassTimer {
    /// Nanoseconds of each pass in [`SoakPass::ALL`] order, then clear the samples.
    pub fn take(&mut self) -> [u64; 5] {
        if self.taken {
            return [0; 5];
        }
        self.taken = true;
        self.last
    }

    /// The clock the profiler probed, or wall when the ring could not be reserved.
    pub fn clock_kind(&self) -> ClockKind {
        self.profiler
            .as_ref()
            .map(TickProfiler::clock_kind)
            .unwrap_or(ClockKind::Wall)
    }
}

impl SoakObserver for PassTimer {
    fn begin(&mut self, pass: SoakPass) {
        let Some(profiler) = self.profiler.as_mut() else {
            return;
        };
        if pass == SoakPass::Step {
            profiler.begin_tick();
        }
        // Names are the const list. An unknown name cannot occur.
        let _ = profiler.begin(pass.name());
    }

    fn end(&mut self, pass: SoakPass) {
        let Some(profiler) = self.profiler.as_mut() else {
            return;
        };
        let _ = profiler.end(pass.name());
        if pass == SoakPass::Step {
            profiler.end_tick(self.ticks);
            self.last = profiler.last_system_ns();
            self.taken = false;
            self.ticks = self.ticks.saturating_add(1);
        }
    }
}

/// Full report of a warmup followed by measured ticks, with per-tick state hashes.
#[derive(Debug, Clone)]
pub struct SoakMeasurement {
    /// Settings captured with this measurement.
    pub config: SoakConfig,
    /// Total resolved contacts, including warmup.
    pub contacts: u64,
    /// Total recycled handles, including warmup.
    pub recycled: u64,
    /// The seed, rate and total ticks (warmup plus measured).
    pub run: DetRun,
    /// Untimed ticks at the beginning of the run.
    pub warmup: u32,
    /// Threshold applied to every pass, nanoseconds.
    pub budget_ns: u64,
    /// `"thread_cpu"` or `"wall"`, from the profiler clock.
    pub clock: ClockKind,
    /// Per-pass timings, indexed by [`SoakPass::index`].
    pub passes: [PassSummary; 5],
    /// The deterministic trace, including warmup; hashing is outside timing.
    pub trace: DetTrace,
}

/// Invalid measurement range (no measured samples or warmup + samples overflow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementError;
impl MeasurementError {
    /// Stable protocol code.
    pub const fn code(self) -> &'static str {
        "invalid_soak_measurement"
    }
}
impl std::fmt::Display for MeasurementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid_soak_measurement: require measured ticks > 0 and warmup + measured <= u32::MAX")
    }
}
impl std::error::Error for MeasurementError {}

/// Reset and measure `run.ticks` ticks after `warmup` untimed ticks.
/// Hashing, sorting, allocation of sample buffers and JSON output are outside timing.
/// The complete step includes the small overhead of four inner timer pairs.
pub fn measure(
    scenario: &mut ScaleScenario,
    run: DetRun,
    warmup: u32,
    budget_ns: u64,
) -> Result<SoakMeasurement, MeasurementError> {
    let total = run
        .ticks
        .checked_add(warmup)
        .filter(|_| run.ticks > 0)
        .ok_or(MeasurementError)?;
    let total_run = DetRun { ticks: total, ..run };
    scenario.reset(&total_run);
    let mut hashes = Vec::with_capacity(total as usize);
    let mut samples: [Vec<u64>; 5] = std::array::from_fn(|_| Vec::with_capacity(run.ticks as usize));
    let mut timer = PassTimer::default();
    let clock = timer.clock_kind();
    for tick in 0..total {
        if tick < warmup {
            scenario.step(tick);
        } else {
            scenario.step_observed(tick, &mut timer);
            for (values, ns) in samples.iter_mut().zip(timer.take()) {
                values.push(ns);
            }
        }
        hashes.push(scenario.fingerprint());
    }
    let passes =
        samples.map(|values| PassSummary::from_samples(&values, budget_ns).expect("positive sample count"));
    Ok(SoakMeasurement {
        config: scenario.config(),
        contacts: scenario.contacts(),
        recycled: scenario.recycled(),
        run: total_run,
        warmup,
        budget_ns,
        clock,
        passes,
        trace: DetTrace {
            scenario: scenario.name().into(),
            run: total_run,
            tick_hashes: hashes,
            planted: scenario.planted(),
        },
    })
}

impl SoakMeasurement {
    /// True if any measured pass (including the whole step) exceeded the budget.
    pub fn over_budget(&self) -> bool {
        self.passes.iter().any(|p| !p.over_budget.is_empty())
    }
    /// One JSON object, without [`REPORT_PREFIX`]. Hashes are fixed-width lowercase hex.
    /// `over_budget_ticks` gives absolute scenario ticks, not warmup-relative indices.
    /// `clock` is `"thread_cpu"` or `"wall"`.
    pub fn to_json(&self) -> String {
        let passes = SoakPass::ALL.into_iter().map(|pass| {
            let p = &self.passes[pass.index()];
            let ticks = p.over_budget.iter().map(|&i| (u64::from(self.warmup) + i as u64).to_string()).collect::<Vec<_>>().join(",");
            format!("{{\"pass\":\"{}\",\"samples\":{},\"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"over_budget_count\":{},\"over_budget_ticks\":[{}]}}",
                pass.name(), p.samples, p.p50_ns, p.p99_ns, p.max_ns, p.over_budget.len(), ticks)
        }).collect::<Vec<_>>().join(",");
        let c = self.config;
        format!(
            "{{\"scenario\":\"scale_soak\",\"clock\":\"{}\",\"target\":\"{}\",\"seed\":\"{:016x}\",\"seats\":{},\"entities\":{},\"half_extent\":{},\"radius\":{},\"speed\":{},\"recycle_interval\":{},\"ticks\":{},\"warmup\":{},\"rate_hz\":{},\"budget_ns\":{},\"status\":\"{}\",\"hash\":\"{:016x}\",\"contacts\":{},\"recycled\":{},\"passes\":[{}]}}",
            self.clock.as_str(),
            current_target(),
            self.run.seed,
            c.seats,
            c.entities,
            c.half_extent,
            c.radius,
            c.speed,
            c.recycle_interval,
            self.run.ticks - self.warmup,
            self.warmup,
            self.run.rate.hz(),
            self.budget_ns,
            if self.over_budget() { "over_budget" } else { "ok" },
            self.trace.run_hash(),
            self.contacts,
            self.recycled,
            passes
        )
    }
}
