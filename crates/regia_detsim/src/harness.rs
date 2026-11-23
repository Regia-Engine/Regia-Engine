// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/harness.rs, commit d6ed37677), ADR-8012.
// Adapt: host-owned bootstrap/state through F-02, portable full-state hash, bounded
// u32 tick numbering and recoverable errors instead of unbounded stepping (ADR-8151).

//! Seed → host bootstrap → fixed steps → fingerprint, using F-02 scenarios.
//!
//! [`SimHarness`] calls [`DetScenario::reset`] exactly once on construction. The
//! scenario owns bootstrap and hashes every field affecting later ticks. No wall
//! time, IO or presentation state belongs in that hash. [`crate::golden::record`]
//! remains the API for a stored per-tick trace; [`crate::soak`] compares live runs
//! without retaining those traces.

use crate::fpu::FpEnvWatch;
use crate::golden::{DetError, DetRun, DetScenario, StateHasher, validate_name};
use crate::state::{self, DetReadLimits, DetState, DetStateError, DetWriter, SnapshotKind};
use crate::tick::TickRate;

/// A recoverable orchestration error with a stable agent code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// A scenario's name violates the F-02 name rules.
    Scenario(DetError),
    /// A step batch exceeds the run's remaining tick budget.
    TickBudget,
    /// A soak must compare at least one pair of independent bootstraps.
    EmptySoak,
    /// Profiling needs at least one measured tick.
    EmptyProfile,
    /// Warmup plus measured ticks does not fit F-02's u32 tick numbering.
    TickOverflow,
    /// Reserving storage for profile samples failed.
    AllocationFailed,
    /// Arena settings are invalid or not finite.
    InvalidArenaSettings,
}

impl HarnessError {
    /// Stable lowercase ASCII code for machine-readable reports.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Scenario(error) => error.code(),
            Self::TickBudget => "tick_budget",
            Self::EmptySoak => "empty_soak",
            Self::EmptyProfile => "empty_profile",
            Self::TickOverflow => "tick_overflow",
            Self::AllocationFailed => "allocation_failed",
            Self::InvalidArenaSettings => "invalid_arena_settings",
        }
    }
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Self::Scenario(error) = self {
            write!(f, "{error}")
        } else {
            f.write_str(self.code())
        }
    }
}

impl std::error::Error for HarnessError {}

/// Snapshot at a tick boundary. Equality is a digest check, not a proof against
/// hash collisions. Fingerprints use the unchanged F-02 [`StateHasher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimSnapshot {
    /// Number of completed steps; zero denotes the bootstrap.
    pub tick: u32,
    /// Hash of all state supplied by [`DetScenario::hash_state`].
    pub fingerprint: u64,
}

/// Owns a scenario and a bounded run, with controlled tick advancement.
pub struct SimHarness<S> {
    state: S,
    run: DetRun,
    tick: u32,
    fp: FpEnvWatch,
}

impl<S: DetScenario> SimHarness<S> {
    /// Bootstrap from the run's seed on the calling thread, after restoring the
    /// default floating-point environment. Validates the scenario name first.
    pub fn new(mut state: S, run: DetRun) -> Result<Self, HarnessError> {
        validate_name(state.name()).map_err(HarnessError::Scenario)?;
        crate::fpu::ensure_default_fp_env();
        state.reset(&run);
        Ok(Self {
            state,
            run,
            tick: 0,
            fp: FpEnvWatch::default(),
        })
    }

    /// Advance one tick, rejecting a completed run without changing state.
    pub fn step(&mut self) -> Result<(), HarnessError> {
        self.step_n(1)
    }

    /// Advance exactly `n` ticks. A batch that exceeds the remaining budget is
    /// rejected before any mutation; zero is a valid no-op, even at the end.
    pub fn step_n(&mut self, n: u32) -> Result<(), HarnessError> {
        if n > self.remaining() {
            return Err(HarnessError::TickBudget);
        }
        for _ in 0..n {
            let _span = tracing::trace_span!("detsim_step", tick = self.tick).entered();
            // ADR-8840: the defined points of the periodic FP-environment check. Foreign code that
            // ran on this thread since the last step is undone before this one starts; a change
            // made inside the step is found right after it.
            self.fp.before_step(u64::from(self.tick));
            self.state.step(self.tick);
            self.fp.after_step(u64::from(self.tick));
            self.tick += 1;
        }
        Ok(())
    }

    /// The periodic floating-point environment check that brackets every step (ADR-8840): how
    /// many checks ran, how many found a changed environment and put the default back, and
    /// whether a change happened during a step ([`FpEnvWatch::during_step`]).
    pub const fn fp_env(&self) -> &FpEnvWatch {
        &self.fp
    }

    /// Set the cadence of the periodic check ([`FpEnvWatch::new`]: `1` every step, the default;
    /// `0` only the guard at construction). Resets the counters.
    pub fn set_fp_check_every(&mut self, every: u32) {
        self.fp = FpEnvWatch::new(every);
    }

    /// Number of completed steps.
    pub const fn tick(&self) -> u32 {
        self.tick
    }

    /// Seed used for the bootstrap.
    pub const fn seed(&self) -> u64 {
        self.run.seed
    }

    /// Complete run configuration.
    pub const fn run(&self) -> DetRun {
        self.run
    }

    /// Steps left before the run is complete.
    pub const fn remaining(&self) -> u32 {
        self.run.ticks - self.tick
    }

    /// Capture the current state without advancing it.
    pub fn snapshot(&self) -> SimSnapshot {
        let _span = tracing::trace_span!("detsim_snapshot", tick = self.tick).entered();
        let mut hasher = StateHasher::new();
        self.state.hash_state(&mut hasher);
        SimSnapshot {
            tick: self.tick,
            fingerprint: hasher.finish(),
        }
    }

    /// Immutable access to the scenario's host-owned state.
    pub const fn state(&self) -> &S {
        &self.state
    }

    /// Return the scenario at the current tick.
    pub fn into_inner(self) -> S {
        self.state
    }
}

/// Saving and resuming a whole run (DC-3.7a).
///
/// [`SimHarness::save`] frames the run (seed, tick budget, rate), the completed tick count and the
/// scenario's [`DetState`] body under the scenario's tag and version; [`SimHarness::resume`] gives
/// back a harness that steps on to the same per-tick hashes as the run that was saved.
impl<S: DetScenario + DetState> SimHarness<S> {
    /// Save the run at its current tick boundary. Never changes the state.
    pub fn save(&self) -> Result<Vec<u8>, DetStateError> {
        let _span = tracing::trace_span!("detsim_harness_save", tick = self.tick).entered();
        let mut w = DetWriter::new();
        w.u64(self.run.seed);
        w.u32(self.run.ticks);
        self.run.rate.save(&mut w);
        w.u32(self.tick);
        self.state.save(&mut w);
        state::frame(
            SnapshotKind::Harness,
            S::STATE_TAG,
            S::STATE_VERSION,
            u64::from(self.tick),
            w,
        )
    }

    /// [`SimHarness::resume_with`] under [`DetReadLimits::DEFAULT`].
    pub fn resume(bytes: &[u8]) -> Result<Self, DetStateError> {
        Self::resume_with(bytes, DetReadLimits::DEFAULT)
    }

    /// Rebuild a harness from [`SimHarness::save`]'s bytes, on the calling thread after restoring
    /// the default floating-point environment. Refuses a frame of another kind, tag or version, a
    /// damaged frame, a tick past the run's budget or different from the header's, a scenario name
    /// that breaks the F-02 rules, and unread bytes. Does not call [`DetScenario::reset`].
    pub fn resume_with(bytes: &[u8], limits: DetReadLimits) -> Result<Self, DetStateError> {
        let _span = tracing::trace_span!("detsim_harness_resume", len = bytes.len()).entered();
        let (frame_tick, mut r) = state::open_frame(
            bytes,
            limits,
            SnapshotKind::Harness,
            S::STATE_TAG,
            S::STATE_VERSION,
        )?;
        let run = DetRun::new(r.u64()?, r.u32()?, TickRate::load(&mut r)?);
        let tick = r.u32()?;
        if tick > run.ticks || u64::from(tick) != frame_tick {
            return Err(DetStateError::bad_value(format_args!(
                "harness tick {tick} (header {frame_tick}) in a run of {} ticks",
                run.ticks
            )));
        }
        let state = S::load(&mut r)?;
        r.finish()?;
        validate_name(state.name())
            .map_err(|e| DetStateError::bad_value(format_args!("scenario name: {e}")))?;
        crate::fpu::ensure_default_fp_env();
        Ok(Self {
            state,
            run,
            tick,
            fp: FpEnvWatch::default(),
        })
    }
}

#[cfg(test)]
mod fp_env_tests {
    //! ADR-8840: the periodic FP-environment check at the harness's step boundaries.

    use super::SimHarness;
    use crate::fpu::{self, FpCheckPoint};
    use crate::golden::{DetRun, DetScenario, StateHasher, TickRate};
    use std::hint::black_box;

    /// Adds a subnormal product to its state every step: a step under flush-to-zero adds `0.0`
    /// instead, so the state (and its hash) shows which steps ran under the IEEE default.
    #[derive(Default)]
    struct SubnormalProbe {
        sum_bits: u32,
        flushed: u32,
        /// Model a foreign library called from inside the step of this tick, which sets FTZ after
        /// the step's own float work.
        plant_during: Option<u32>,
    }

    impl DetScenario for SubnormalProbe {
        fn name(&self) -> &str {
            "fp_env_probe"
        }

        fn default_run(&self) -> DetRun {
            DetRun::new(1, 8, TickRate::DEFAULT)
        }

        fn reset(&mut self, _run: &DetRun) {
            self.sum_bits = 0;
            self.flushed = 0;
        }

        fn step(&mut self, tick: u32) {
            let sub = black_box(f32::MIN_POSITIVE) * black_box(0.5);
            if sub == 0.0 {
                self.flushed += 1;
            }
            self.sum_bits = (f32::from_bits(self.sum_bits) + sub).to_bits();
            if self.plant_during == Some(tick) {
                fpu::set_flush_to_zero_for_tests();
            }
        }

        fn hash_state(&self, h: &mut StateHasher) {
            h.u32("sum_bits", self.sum_bits);
            h.u32("flushed", self.flushed);
        }
    }

    fn clean_run() -> SimHarness<SubnormalProbe> {
        let run = DetRun::new(1, 8, TickRate::DEFAULT);
        let mut harness = SimHarness::new(SubnormalProbe::default(), run).expect("scenario");
        harness.step_n(8).expect("budget");
        harness
    }

    /// The brief's case: a foreign library on the sim thread flips FTZ between two steps. The
    /// next step must still run under the IEEE default (same state and hash as a clean run), and
    /// the harness must say so.
    #[test]
    fn foreign_ftz_between_steps_is_caught_and_reset() {
        let clean = clean_run();
        assert_eq!(clean.state().flushed, 0);
        let run = DetRun::new(1, 8, TickRate::DEFAULT);
        let mut dirty = SimHarness::new(SubnormalProbe::default(), run).expect("scenario");
        dirty.step_n(3).expect("budget");
        // The foreign library: an audio or video call on the sim thread between two steps.
        let planted = fpu::set_flush_to_zero_for_tests();
        dirty.step_n(5).expect("budget");
        // Put the test thread back before anything can unwind with FTZ still set.
        let left = fpu::ensure_default_fp_env();
        assert!(planted.was_default(), "{planted:?}");
        assert!(left.was_default(), "the harness left the default: {left:?}");
        assert_eq!(
            dirty.state().flushed,
            0,
            "a step after the foreign call ran under flush-to-zero"
        );
        assert_eq!(dirty.snapshot(), clean.snapshot());

        let supported = planted.supported;
        let watch = dirty.fp_env();
        assert_eq!(
            watch.checks(),
            16,
            "one check before and one after each of 8 steps"
        );
        assert_eq!(watch.corrections(), u64::from(supported));
        assert_eq!(watch.during_step(), 0);
        if supported {
            let change = watch.first_change().expect("the planted FTZ is found");
            assert_eq!((change.tick, change.point), (3, FpCheckPoint::BeforeStep));
            assert!(change.report.flush_to_zero, "{change:?}");
        }
    }

    /// A change made inside a step is found right after that step (a desync risk the host must
    /// see), and the following steps run under the default again.
    #[test]
    fn ftz_set_during_a_step_is_named_and_does_not_leak() {
        let run = DetRun::new(1, 8, TickRate::DEFAULT);
        let probe = SubnormalProbe {
            plant_during: Some(4),
            ..SubnormalProbe::default()
        };
        let mut harness = SimHarness::new(probe, run).expect("scenario");
        harness.step_n(8).expect("budget");
        let left = fpu::ensure_default_fp_env();
        assert!(left.was_default(), "{left:?}");
        // The plant comes after tick 4's float work, so no step flushed.
        assert_eq!(harness.state().flushed, 0);
        assert_eq!(harness.snapshot(), clean_run().snapshot());
        let supported = left.supported;
        let watch = harness.fp_env();
        assert_eq!(watch.during_step(), u64::from(supported));
        assert_eq!(watch.corrections(), u64::from(supported));
        if supported {
            let change = watch.last_change().expect("found");
            assert_eq!((change.tick, change.point), (4, FpCheckPoint::AfterStep));
        }
    }

    /// With the periodic check off, the guard at construction is all there is: a change between
    /// steps reaches the following steps. This is what the default cadence prevents.
    #[test]
    fn with_the_check_off_a_foreign_ftz_reaches_the_sim() {
        let run = DetRun::new(1, 8, TickRate::DEFAULT);
        let mut harness = SimHarness::new(SubnormalProbe::default(), run).expect("scenario");
        harness.set_fp_check_every(0);
        harness.step_n(3).expect("budget");
        let planted = fpu::set_flush_to_zero_for_tests();
        harness.step_n(5).expect("budget");
        let left = fpu::ensure_default_fp_env();
        assert!(planted.was_default());
        assert_eq!(harness.fp_env().checks(), 0);
        if left.supported {
            assert!(left.flush_to_zero, "nothing reset it: {left:?}");
            assert_eq!(harness.state().flushed, 5, "steps 3..8 ran under FTZ");
            assert_ne!(harness.snapshot(), clean_run().snapshot());
        }
    }
}
