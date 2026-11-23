// SPDX-License-Identifier: MPL-2.0
// Portions of `run` follow Direct Order 27 (ir-sim/src/profile_alpha.rs, commit d6ed37677), ADR-8012.
// Adapt: host scenario instead of game maps; explicit warmup; reject zero samples;
// integer nearest-rank percentiles, nanoseconds, full F-02 state hash (ADR-8151).
// The ring, the per-system rows and the thread clock are original (ADR-8498). Nearest rank
// is the NIST/SEMATECH definition, not copied arithmetic.

//! Per-system tick profiler, kept outside deterministic state (ADR-8498).
//!
//! [`TickProfiler`] is a separate struct. It does not read or write simulation state, so
//! wrapping the same scenario with it cannot move a desync hash. The system names are the
//! same [`Systems`] list the hash uses, so one list names both.
//!
//! Samples are a fixed ring (default [`DEFAULT_RING_TICKS`] ticks). `begin`, `end`,
//! `end_tick` and the guard allocate nothing after construction. [`TickProfiler::report`]
//! may allocate; that is the publish path, not the tick path.
//!
//! The clock is per-thread CPU time when the operating system provides it, and monotonic
//! wall time otherwise. A [`ManualClock`] injects samples in tests. Neither clock is read
//! from a lockstep step function.
//!
//! A tracing span `detsim_system` is entered per open system only while an INFO subscriber
//! is installed. The entered span lives in a thread-local slot, not in this struct, so the
//! profiler stays [`Send`]. With no subscriber the tick path does not touch that slot.

#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the span-slot initializer is already const (ADR-8498)"
    )
)]

use std::cell::RefCell;

use serde::{Deserialize, Serialize};

use crate::golden::{DetRun, DetScenario, TickRate};
use crate::harness::{HarnessError, SimHarness, SimSnapshot};
use crate::hash::{MAX_SYSTEMS, Systems};

/// Default ring length: 3600 ticks (60 s at 60 Hz).
pub const DEFAULT_RING_TICKS: usize = 3600;

/// Default whole-tick and per-system budget: 4.0 ms, in nanoseconds.
pub const DEFAULT_TICK_BUDGET_NS: u64 = 4_000_000;

/// Largest ring [`TickProfiler::new`] will reserve.
pub const MAX_RING_TICKS: usize = 1_048_576;

/// Most over-budget ticks `sim.tick_profile` returns in one answer. Matches the default ring.
pub const MAX_REPORT_TICKS: usize = DEFAULT_RING_TICKS;

/// Which clock produced the nanoseconds in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockKind {
    /// Per-thread CPU time (`clock_gettime` thread clock, or `GetThreadTimes` on Windows).
    ThreadCpu,
    /// Monotonic elapsed time. Used when the thread clock is missing, and by [`ManualClock`]
    /// unless a test selects [`ClockKind::ThreadCpu`].
    Wall,
}

impl ClockKind {
    /// The report string: `"thread_cpu"` or `"wall"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreadCpu => "thread_cpu",
            Self::Wall => "wall",
        }
    }
}

/// A clock the profiler can sample. Implementations must not touch simulation state.
pub trait ProfileClock: Send {
    /// Which kind [`ProfileClock::now_ns`] reports.
    fn kind(&self) -> ClockKind;

    /// Monotonic nanoseconds from an arbitrary origin. A step backward becomes a zero delta.
    fn now_ns(&self) -> u64;
}

/// The operating system's thread CPU clock, or monotonic wall time if the probe fails.
///
/// Probe on the thread that will call `begin` and `end`. The kind does not change after that.
#[derive(Debug)]
pub struct HostClock {
    kind: ClockKind,
    wall_origin: std::time::Instant,
    last_cpu: std::cell::Cell<u64>,
}

impl HostClock {
    /// Read the thread clock once. `Some` selects [`ClockKind::ThreadCpu`]; `None` selects wall.
    pub fn probe() -> Self {
        let sample = crate::platform::thread_cpu_now_ns();
        let kind = if sample.is_some() {
            ClockKind::ThreadCpu
        } else {
            ClockKind::Wall
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "ADR-8151: the profiler's wall-clock fallback times steps outside the deterministic state; nothing simulated reads it"
        )]
        // det-scan: allow(wall_clock): profiling only, never hashed
        let wall_origin = std::time::Instant::now();
        Self {
            kind,
            wall_origin,
            last_cpu: std::cell::Cell::new(sample.unwrap_or(0)),
        }
    }

    /// The kind chosen by [`HostClock::probe`].
    pub fn kind(&self) -> ClockKind {
        self.kind
    }
}

impl ProfileClock for HostClock {
    fn kind(&self) -> ClockKind {
        self.kind
    }

    fn now_ns(&self) -> u64 {
        match self.kind {
            ClockKind::ThreadCpu => {
                if let Some(ns) = crate::platform::thread_cpu_now_ns() {
                    self.last_cpu.set(ns);
                    ns
                } else {
                    self.last_cpu.get()
                }
            }
            ClockKind::Wall => wall_elapsed_ns(self.wall_origin),
        }
    }
}

fn wall_elapsed_ns(origin: std::time::Instant) -> u64 {
    // det-scan: allow(wall_clock): profiling only, never hashed
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A clock whose readings the caller sets. Tests use it so quantiles do not depend on time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    kind: ClockKind,
    now: u64,
}

impl ManualClock {
    /// A clock that starts at 0 ns and reports `kind`.
    pub const fn new(kind: ClockKind) -> Self {
        Self { kind, now: 0 }
    }

    /// Replace the current reading.
    pub const fn set(&mut self, now_ns: u64) {
        self.now = now_ns;
    }

    /// Add `delta_ns` to the current reading. Saturates at `u64::MAX`.
    pub const fn advance(&mut self, delta_ns: u64) {
        self.now = self.now.saturating_add(delta_ns);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(ClockKind::Wall)
    }
}

impl ProfileClock for ManualClock {
    fn kind(&self) -> ClockKind {
        self.kind
    }

    fn now_ns(&self) -> u64 {
        self.now
    }
}

/// Ring length and budgets. A sample is over budget only when it is strictly greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSettings {
    /// Retained ticks. `1..=MAX_RING_TICKS`. The default is [`DEFAULT_RING_TICKS`].
    pub ring_ticks: usize,
    /// Whole-tick budget in nanoseconds. The default is [`DEFAULT_TICK_BUDGET_NS`] (4.0 ms).
    pub tick_budget_ns: u64,
    /// Budget applied to every system, in nanoseconds. The default is 4.0 ms.
    pub system_budget_ns: u64,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            ring_ticks: DEFAULT_RING_TICKS,
            tick_budget_ns: DEFAULT_TICK_BUDGET_NS,
            system_budget_ns: DEFAULT_TICK_BUDGET_NS,
        }
    }
}

/// Why a profiler could not be built, or a system name was not on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    /// `begin` or `end` was given a name that is not on the [`Systems`] list.
    UnknownSystem,
    /// `ring_ticks` was 0.
    EmptyRing,
    /// `ring_ticks` was above [`MAX_RING_TICKS`].
    RingTooLarge,
    /// Reserving the ring failed.
    AllocationFailed,
}

impl ProfileError {
    /// Stable lowercase code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnknownSystem => "unknown_system",
            Self::EmptyRing => "empty_ring",
            Self::RingTooLarge => "ring_too_large",
            Self::AllocationFailed => "allocation_failed",
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ProfileError {}

#[derive(Debug, Clone, Copy)]
struct TickSample<const N: usize> {
    tick: u64,
    total_ns: u64,
    systems: [u64; N],
}

impl<const N: usize> Default for TickSample<N> {
    fn default() -> Self {
        Self {
            tick: 0,
            total_ns: 0,
            systems: [0; N],
        }
    }
}

thread_local! {
    static SPANS: RefCell<[Option<tracing::span::EnteredSpan>; MAX_SYSTEMS]> =
        const { RefCell::new([const { None }; MAX_SYSTEMS]) };
}

fn push_span(index: usize, name: &'static str) -> bool {
    if index >= MAX_SYSTEMS || !tracing::enabled!(tracing::Level::INFO) {
        return false;
    }
    let entered = tracing::info_span!("detsim_system", system = name).entered();
    SPANS.with(|slots| {
        let mut slots = slots.borrow_mut();
        if slots[index].is_some() {
            drop(entered);
            return false;
        }
        slots[index] = Some(entered);
        true
    })
}

fn pop_span(index: usize) {
    if index >= MAX_SYSTEMS || !tracing::enabled!(tracing::Level::INFO) {
        return;
    }
    SPANS.with(|slots| {
        slots.borrow_mut()[index] = None;
    });
}

fn clear_spans() {
    if !tracing::enabled!(tracing::Level::INFO) {
        return;
    }
    SPANS.with(|slots| {
        for slot in slots.borrow_mut().iter_mut() {
            *slot = None;
        }
    });
}

/// Per-system timings for one name list, over a fixed ring of ticks.
///
/// `N` is the length of `systems`. Call `begin_tick` at the start of a tick, `begin` / `end`
/// (or [`TickProfiler::enter`]) around each system, then `end_tick`. The whole-tick time is
/// the clock span from `begin_tick` to `end_tick`, not the sum of the systems, so a system
/// that nests others is not added twice into the tick total. Each system's own time is the
/// span from its `begin` to its `end` and does include nested work.
#[derive(Debug)]
pub struct TickProfiler<const N: usize, C: ProfileClock> {
    systems: Systems<N>,
    clock: C,
    settings: ProfileSettings,
    samples: Vec<TickSample<N>>,
    head: usize,
    valid: usize,
    origin: u64,
    open: [Option<u64>; N],
    system_ns: [u64; N],
    spanned: [bool; N],
}

/// Ends one system when dropped.
#[must_use = "the guard ends the system when dropped"]
pub struct SystemGuard<'a, const N: usize, C: ProfileClock> {
    profiler: &'a mut TickProfiler<N, C>,
    index: usize,
    active: bool,
}

impl<const N: usize, C: ProfileClock> Drop for SystemGuard<'_, N, C> {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let now = self.profiler.clock.now_ns();
            self.profiler.finish_open(self.index, now);
        }
    }
}

impl<'a, const N: usize, C: ProfileClock> SystemGuard<'a, N, C> {
    /// The clock, so a [`ManualClock`] can advance while this system is open.
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.profiler.clock
    }
}

impl<const N: usize, C: ProfileClock> TickProfiler<N, C> {
    /// Reserve the ring. Fails on an empty ring, a ring above [`MAX_RING_TICKS`], or if the
    /// reservation fails. The reservation is the only allocation `begin` / `end` / `end_tick` need.
    pub fn new(systems: Systems<N>, clock: C, settings: ProfileSettings) -> Result<Self, ProfileError> {
        if settings.ring_ticks == 0 {
            return Err(ProfileError::EmptyRing);
        }
        if settings.ring_ticks > MAX_RING_TICKS {
            return Err(ProfileError::RingTooLarge);
        }
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(settings.ring_ticks)
            .map_err(|_| ProfileError::AllocationFailed)?;
        samples.resize(settings.ring_ticks, TickSample::default());
        let origin = clock.now_ns();
        Ok(Self {
            systems,
            clock,
            settings,
            samples,
            head: 0,
            valid: 0,
            origin,
            open: [None; N],
            system_ns: [0; N],
            spanned: [false; N],
        })
    }

    /// The clock, so a test can move a [`ManualClock`] between `begin` and `end`.
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    /// The kind the clock reports.
    pub fn clock_kind(&self) -> ClockKind {
        self.clock.kind()
    }

    /// The name list, in vector order.
    pub fn systems(&self) -> &Systems<N> {
        &self.systems
    }

    /// Start a tick. The whole-tick span runs from this call to [`TickProfiler::end_tick`].
    /// A system left open from an unfinished tick is dropped and not recorded.
    pub fn begin_tick(&mut self) {
        self.abandon_open();
        self.system_ns = [0; N];
        self.origin = self.clock.now_ns();
    }

    /// Open `system`. A second `begin` while it is already open is ignored.
    /// An unknown name is [`ProfileError::UnknownSystem`].
    pub fn begin(&mut self, system: &str) -> Result<(), ProfileError> {
        let index = self.index_of(system)?;
        self.open_index(index);
        Ok(())
    }

    /// Close `system` and add the elapsed nanoseconds. `end` without `begin` does nothing.
    /// An unknown name is [`ProfileError::UnknownSystem`].
    pub fn end(&mut self, system: &str) -> Result<(), ProfileError> {
        let index = self.index_of(system)?;
        let now = self.clock.now_ns();
        self.finish_open(index, now);
        Ok(())
    }

    /// Open `system` and close it when the guard drops. A second open is an inactive guard
    /// that does not close the system already running.
    pub fn enter(&mut self, system: &str) -> Result<SystemGuard<'_, N, C>, ProfileError> {
        let index = self.index_of(system)?;
        let active = self.open_index(index);
        Ok(SystemGuard {
            profiler: self,
            index,
            active,
        })
    }

    /// Close any system still open, record this tick, and advance the ring.
    /// The oldest tick is overwritten once the ring is full. Allocates nothing.
    pub fn end_tick(&mut self, tick: u64) {
        let now = self.clock.now_ns();
        self.close_open(now);
        let total_ns = now.saturating_sub(self.origin);
        let sample = TickSample {
            tick,
            total_ns,
            systems: self.system_ns,
        };
        let cap = self.samples.len();
        self.samples[self.head] = sample;
        self.head = (self.head + 1) % cap;
        if self.valid < cap {
            self.valid += 1;
        }
        self.system_ns = [0; N];
        self.origin = now;
        clear_spans();
    }

    /// System nanoseconds of the newest recorded tick, or zeros when none has been recorded.
    pub fn last_system_ns(&self) -> [u64; N] {
        if self.valid == 0 {
            return [0; N];
        }
        let cap = self.samples.len();
        let index = if self.head == 0 { cap - 1 } else { self.head - 1 };
        self.samples[index].systems
    }

    /// Quantiles, budgets and every retained over-budget tick. This may allocate.
    pub fn report(&self) -> ProfileReport {
        let chrono = self.chronological();
        let mut systems = Vec::with_capacity(N);
        for index in 0..N {
            let mut column = Vec::with_capacity(chrono.len());
            for sample in &chrono {
                column.push(sample.systems[index]);
            }
            column.sort_unstable();
            let name = self.systems.name(index).unwrap_or("system");
            systems.push(SystemProfile {
                system: name.to_owned(),
                budget_ns: self.settings.system_budget_ns,
                p50_ns: nearest_rank(&column, 50),
                p90_ns: nearest_rank(&column, 90),
                p99_ns: nearest_rank(&column, 99),
                max_ns: column.last().copied().unwrap_or(0),
            });
        }
        let mut totals = Vec::with_capacity(chrono.len());
        for sample in &chrono {
            totals.push(sample.total_ns);
        }
        totals.sort_unstable();
        let mut over_budget = Vec::new();
        for sample in &chrono {
            let system_over = sample
                .systems
                .iter()
                .any(|ns| *ns > self.settings.system_budget_ns);
            let tick_over = sample.total_ns > self.settings.tick_budget_ns;
            if !(system_over || tick_over) {
                continue;
            }
            let mut rows = Vec::with_capacity(N);
            for index in 0..N {
                let name = self.systems.name(index).unwrap_or("system");
                rows.push(SystemNanos {
                    system: name.to_owned(),
                    ns: sample.systems[index],
                });
            }
            over_budget.push(OverBudgetTick {
                tick: sample.tick,
                total_ns: sample.total_ns,
                systems: rows,
            });
        }
        ProfileReport {
            clock: self.clock.kind(),
            samples: u32::try_from(self.valid).unwrap_or(u32::MAX),
            tick_budget_ns: self.settings.tick_budget_ns,
            system_budget_ns: self.settings.system_budget_ns,
            systems,
            tick: TickQuantiles {
                p50_ns: nearest_rank(&totals, 50),
                p90_ns: nearest_rank(&totals, 90),
                p99_ns: nearest_rank(&totals, 99),
                max_ns: totals.last().copied().unwrap_or(0),
            },
            over_budget,
        }
    }

    fn index_of(&self, system: &str) -> Result<usize, ProfileError> {
        self.systems.index_of(system).ok_or(ProfileError::UnknownSystem)
    }

    /// Returns whether this call opened the system (false when it was already open).
    fn open_index(&mut self, index: usize) -> bool {
        if self.open[index].is_some() {
            return false;
        }
        let name = self.systems.name(index).unwrap_or("system");
        self.open[index] = Some(self.clock.now_ns());
        self.spanned[index] = push_span(index, name);
        true
    }

    fn finish_open(&mut self, index: usize, now: u64) {
        if let Some(start) = self.open[index].take() {
            self.system_ns[index] = self.system_ns[index].saturating_add(now.saturating_sub(start));
            if self.spanned[index] {
                self.spanned[index] = false;
                pop_span(index);
            }
        }
    }

    fn close_open(&mut self, now: u64) {
        for index in 0..N {
            self.finish_open(index, now);
        }
    }

    fn abandon_open(&mut self) {
        for index in 0..N {
            self.open[index] = None;
            if self.spanned[index] {
                self.spanned[index] = false;
                pop_span(index);
            }
        }
        clear_spans();
    }

    fn chronological(&self) -> Vec<&TickSample<N>> {
        let mut out = Vec::with_capacity(self.valid);
        if self.samples.is_empty() || self.valid == 0 {
            return out;
        }
        let cap = self.samples.len();
        let start = if self.valid == cap { self.head } else { 0 };
        for offset in 0..self.valid {
            let index = (start + offset) % cap;
            out.push(&self.samples[index]);
        }
        out
    }
}

/// Nearest-rank percentile of a sorted slice.
///
/// The rank is `ceil(percent * n / 100)` (1-based). The value is the sample at that rank.
/// An empty slice is 0. This is NIST/SEMATECH e-Handbook of Statistical Methods § 7.2.6.2
/// with integer arithmetic: `(n * percent).div_ceil(100)`.
pub fn nearest_rank(sorted: &[u64], percent: u32) -> u64 {
    let rank = (sorted.len() as u64)
        .saturating_mul(u64::from(percent))
        .div_ceil(100);
    let index = usize::try_from(rank.saturating_sub(1)).unwrap_or(0);
    sorted.get(index).copied().unwrap_or(0)
}

/// One system's quantiles over the ticks the ring still holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemProfile {
    /// System name, from the shared [`Systems`] list.
    pub system: String,
    /// Budget applied to this system, nanoseconds.
    pub budget_ns: u64,
    /// Nearest-rank median, nanoseconds.
    pub p50_ns: u64,
    /// Nearest-rank 90th percentile, nanoseconds.
    pub p90_ns: u64,
    /// Nearest-rank 99th percentile, nanoseconds.
    pub p99_ns: u64,
    /// Slowest retained sample, nanoseconds.
    pub max_ns: u64,
}

/// Whole-tick quantiles over the ticks the ring still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickQuantiles {
    /// Nearest-rank median, nanoseconds.
    pub p50_ns: u64,
    /// Nearest-rank 90th percentile, nanoseconds.
    pub p90_ns: u64,
    /// Nearest-rank 99th percentile, nanoseconds.
    pub p99_ns: u64,
    /// Slowest retained tick, nanoseconds.
    pub max_ns: u64,
}

/// One system's nanoseconds on one over-budget tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemNanos {
    /// System name.
    pub system: String,
    /// Inclusive nanoseconds for that system on this tick.
    pub ns: u64,
}

/// A tick that exceeded the whole-tick budget or any system budget, with every system row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverBudgetTick {
    /// Tick number passed to [`TickProfiler::end_tick`].
    pub tick: u64,
    /// Whole-tick nanoseconds.
    pub total_ns: u64,
    /// Every system on this tick, in list order.
    pub systems: Vec<SystemNanos>,
}

/// The agent-facing report. `samples` is how many ticks the ring still holds, not how many
/// were ever recorded. `over_budget` is oldest first and drops a tick once the ring overwrites it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileReport {
    /// `"thread_cpu"` or `"wall"`.
    pub clock: ClockKind,
    /// Ticks retained in the ring.
    pub samples: u32,
    /// Whole-tick budget, nanoseconds.
    pub tick_budget_ns: u64,
    /// Per-system budget, nanoseconds.
    pub system_budget_ns: u64,
    /// One row per system, in list order.
    pub systems: Vec<SystemProfile>,
    /// Quantiles of the whole tick.
    pub tick: TickQuantiles,
    /// Every retained tick strictly over a system budget or the whole-tick budget.
    pub over_budget: Vec<OverBudgetTick>,
}

/// A bounded timing run of one scenario. Kept for the harness runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileConfig {
    /// Seed passed to the host's bootstrap.
    pub seed: u64,
    /// Simulation tick rate.
    pub rate: TickRate,
    /// Untimed ticks before collecting samples.
    pub warmup_ticks: u32,
    /// Measured ticks, must be nonzero.
    pub samples: u32,
}

/// Sorted tick-cost statistics and a deterministic final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickProfile {
    /// Number of measured steps.
    pub samples: u32,
    /// Median step time in nanoseconds (nearest rank).
    pub p50_ns: u64,
    /// 99th percentile step time in nanoseconds (nearest rank).
    pub p99_ns: u64,
    /// Slowest step in nanoseconds.
    pub max_ns: u64,
    /// Snapshot after warmup plus samples; does not include timing values.
    pub final_snapshot: SimSnapshot,
}

fn profile_to_harness(error: ProfileError) -> HarnessError {
    match error {
        ProfileError::UnknownSystem
        | ProfileError::EmptyRing
        | ProfileError::RingTooLarge
        | ProfileError::AllocationFailed => HarnessError::AllocationFailed,
    }
}

/// Bootstrap once, warm up, then measure each step. Allocation and tick overflow
/// are checked before bootstrap so invalid requests do not run host simulation.
pub fn run<S: DetScenario>(state: S, config: ProfileConfig) -> Result<TickProfile, HarnessError> {
    if config.samples == 0 {
        return Err(HarnessError::EmptyProfile);
    }
    let ticks = config
        .warmup_ticks
        .checked_add(config.samples)
        .ok_or(HarnessError::TickOverflow)?;
    const STEP: Systems<1> = Systems::new(["step"]);
    let settings = ProfileSettings {
        ring_ticks: usize::try_from(config.samples).unwrap_or(usize::MAX),
        tick_budget_ns: u64::MAX,
        system_budget_ns: u64::MAX,
    };
    let mut profiler = TickProfiler::new(STEP, HostClock::probe(), settings).map_err(profile_to_harness)?;
    let _span = tracing::info_span!("detsim_profile", samples = config.samples).entered();
    let mut harness = SimHarness::new(state, DetRun::new(config.seed, ticks, config.rate))?;
    harness.step_n(config.warmup_ticks)?;
    for index in 0..config.samples {
        profiler.begin_tick();
        {
            let _guard = profiler.enter("step").map_err(profile_to_harness)?;
            harness.step()?;
        }
        profiler.end_tick(u64::from(index));
    }
    let report = profiler.report();
    let step = report.systems.first();
    Ok(TickProfile {
        samples: config.samples,
        p50_ns: step.map(|row| row.p50_ns).unwrap_or(0),
        p99_ns: step.map(|row| row.p99_ns).unwrap_or(0),
        max_ns: step.map(|row| row.max_ns).unwrap_or(0),
        final_snapshot: harness.snapshot(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClockKind, HostClock, ManualClock, ProfileError, ProfileSettings, Systems, TickProfiler, nearest_rank,
    };
    use crate::golden::DetScenario;
    use crate::scale_soak::{ScaleScenario, SoakConfig};

    #[test]
    fn nearest_rank_uses_integer_positions_even_for_small_samples() {
        assert_eq!(nearest_rank(&[], 50), 0);
        assert_eq!(nearest_rank(&[7], 99), 7);
        assert_eq!(nearest_rank(&[10, 20], 50), 10);
        assert_eq!(nearest_rank(&[10, 20], 99), 20);
        let hundred: Vec<_> = (1..=100).collect();
        assert_eq!(nearest_rank(&hundred, 50), 50);
        assert_eq!(nearest_rank(&hundred, 99), 99);
        assert_eq!(nearest_rank(&hundred, 100), 100);
    }

    fn profiler(ring: usize, tick_budget_ns: u64, system_budget_ns: u64) -> TickProfiler<2, ManualClock> {
        TickProfiler::new(
            Systems::new(["a", "b"]),
            ManualClock::new(ClockKind::Wall),
            ProfileSettings {
                ring_ticks: ring,
                tick_budget_ns,
                system_budget_ns,
            },
        )
        .expect("ring")
    }

    fn push(profiler: &mut TickProfiler<2, ManualClock>, tick: u64, a_ns: u64, b_ns: u64) {
        profiler.clock_mut().set(0);
        profiler.begin_tick();
        profiler.begin("a").expect("a");
        profiler.clock_mut().advance(a_ns);
        profiler.end("a").expect("a");
        profiler.begin("b").expect("b");
        profiler.clock_mut().advance(b_ns);
        profiler.end("b").expect("b");
        profiler.end_tick(tick);
    }

    #[test]
    fn quantiles_use_nearest_rank_and_keep_every_over_budget_tick() {
        let mut wide = profiler(16, u64::MAX, u64::MAX);
        for tick in 1..=10 {
            push(&mut wide, tick, tick * 10, 0);
        }
        let report = wide.report();
        assert_eq!(report.clock, ClockKind::Wall);
        assert_eq!(report.samples, 10);
        let a = &report.systems[0];
        assert_eq!((a.p50_ns, a.p90_ns, a.p99_ns, a.max_ns), (50, 90, 100, 100));
        assert_eq!(
            (
                report.tick.p50_ns,
                report.tick.p90_ns,
                report.tick.p99_ns,
                report.tick.max_ns
            ),
            (50, 90, 100, 100)
        );
        assert_eq!(report.systems[1].p50_ns, 0);
        assert!(report.over_budget.is_empty());

        let mut pair = profiler(8, u64::MAX, u64::MAX);
        push(&mut pair, 1, 10, 0);
        push(&mut pair, 2, 20, 0);
        let pair = pair.report();
        assert_eq!(pair.systems[0].p50_ns, 10);
        assert_eq!(pair.systems[0].p99_ns, 20);

        // Several ticks over a system budget, including ones a p99 alone would hide.
        let mut flagged = profiler(16, 1_000, 55);
        for tick in 1..=10 {
            push(&mut flagged, tick, tick * 10, 0);
        }
        let flagged = flagged.report();
        let ticks: Vec<u64> = flagged.over_budget.iter().map(|row| row.tick).collect();
        assert_eq!(ticks, vec![6, 7, 8, 9, 10]);
        let last = flagged.over_budget.last().expect("last over-budget tick");
        assert_eq!(last.total_ns, 100);
        assert_eq!(last.systems[0].ns, 100);
        assert_eq!(last.systems[1].ns, 0);

        // Equal to the budget is not over. The next nanosecond is.
        let mut edge = profiler(4, 55, 55);
        push(&mut edge, 1, 55, 0);
        push(&mut edge, 2, 56, 0);
        let ticks: Vec<u64> = edge.report().over_budget.iter().map(|row| row.tick).collect();
        assert_eq!(ticks, vec![2]);

        // A whole tick can be over while each system stays under its own budget.
        let mut tick_only = profiler(4, 15, 1_000);
        push(&mut tick_only, 1, 10, 0);
        push(&mut tick_only, 2, 20, 0);
        let ticks: Vec<u64> = tick_only
            .report()
            .over_budget
            .iter()
            .map(|row| row.tick)
            .collect();
        assert_eq!(ticks, vec![2]);

        // The ring forgets a scrolled-off over-budget tick and keeps every one it still holds.
        let mut ring = profiler(4, 50, u64::MAX);
        for (tick, total) in [(1, 100), (2, 10), (3, 10), (4, 10), (5, 10), (6, 100)] {
            push(&mut ring, tick, total, 0);
        }
        let wrapped = ring.report();
        assert_eq!(wrapped.samples, 4);
        let ticks: Vec<u64> = wrapped.over_budget.iter().map(|row| row.tick).collect();
        assert_eq!(ticks, vec![6]);

        let mut guard_clock = profiler(2, u64::MAX, u64::MAX);
        guard_clock.clock_mut().set(10);
        guard_clock.begin_tick();
        {
            let mut guard = guard_clock.enter("b").expect("b");
            guard.clock_mut().advance(7);
        }
        guard_clock.end_tick(4);
        let report = guard_clock.report();
        assert_eq!(report.systems[1].max_ns, 7);
        assert_eq!(report.tick.max_ns, 7);

        let mut unknown = profiler(2, 1, 1);
        assert_eq!(unknown.begin("nope"), Err(ProfileError::UnknownSystem));
        assert_eq!(unknown.end("nope"), Err(ProfileError::UnknownSystem));
        assert_eq!(
            TickProfiler::<2, ManualClock>::new(
                Systems::new(["a", "b"]),
                ManualClock::default(),
                ProfileSettings {
                    ring_ticks: 0,
                    ..ProfileSettings::default()
                },
            )
            .expect_err("empty")
            .code(),
            "empty_ring"
        );
    }

    #[test]
    fn profiler_does_not_change_the_hash() {
        fn hashes(profile: bool) -> Vec<u64> {
            let config = SoakConfig {
                seats: 2,
                entities: 8,
                ..SoakConfig::default()
            };
            let mut scenario = ScaleScenario::new(config).expect("config");
            let mut profiler = profile.then(|| {
                TickProfiler::new(
                    Systems::new(["step"]),
                    ManualClock::new(ClockKind::Wall),
                    ProfileSettings {
                        ring_ticks: 16,
                        ..ProfileSettings::default()
                    },
                )
                .expect("profiler")
            });
            let mut out = Vec::new();
            for tick in 0u32..8 {
                if let Some(profiler) = profiler.as_mut() {
                    profiler.begin_tick();
                    profiler.begin("step").expect("step");
                }
                scenario.step(tick);
                if let Some(profiler) = profiler.as_mut() {
                    profiler.end("step").expect("step");
                    profiler.end_tick(u64::from(tick));
                }
                out.push(scenario.fingerprint());
            }
            out
        }
        assert_eq!(hashes(false), hashes(true));
    }

    #[test]
    fn thread_clock_is_reported() {
        assert_eq!(
            serde_json::to_value(ClockKind::ThreadCpu).expect("json"),
            serde_json::json!("thread_cpu")
        );
        assert_eq!(
            serde_json::to_value(ClockKind::Wall).expect("json"),
            serde_json::json!("wall")
        );
        let kind = HostClock::probe().kind();
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "windows",
            target_os = "macos"
        ))]
        assert_eq!(kind, ClockKind::ThreadCpu);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "windows",
            target_os = "macos"
        )))]
        assert_eq!(kind, ClockKind::Wall);
    }
}
