// SPDX-License-Identifier: MPL-2.0
//! The FTZ/DAZ guard: put the calling thread's floating-point environment back to the IEEE default
//! before a lockstep sim steps on it (ADR-8012 A-4, ADR-8013).
//!
//! Rust, and every peer of a lockstep match, assumes the default environment: round to nearest even,
//! no exception traps, and subnormal numbers kept (not flushed to zero). A thread can still inherit
//! another one: an audio or video library may set flush-to-zero (FTZ) and denormals-are-zero (DAZ)
//! for speed, and a driver or plugin may change the rounding mode. On such a thread
//! `f32::MIN_POSITIVE * 0.5` is `0.0` instead of a subnormal, and one peer's sim drifts from the
//! others without any error. Call [`ensure_default_fp_env`] at sim-thread start (and, if foreign
//! code runs on the sim thread, after it) to put the environment back and learn what was wrong.
//!
//! - x86-64: MXCSR. FTZ is bit 15, DAZ bit 6, the rounding control bits 13-14 and the exception
//!   masks bits 7-12; the default control value is `0x1F80`. The sticky status flags (bits 0-5)
//!   are kept.
//! - aarch64: FPCR. FZ is bit 24, DN (default NaN) bit 25, FZ16 bit 19, the rounding mode bits 22-23,
//!   the trap enables bits 8-12 and 15, AHP bit 26, and the FEAT_AFP bits FIZ, AH and NEP (0-2); the
//!   default on Linux, Android and macOS is all of them clear. aarch64 has no DAZ: FZ flushes
//!   subnormal inputs and outputs alike.
//! - Other targets (wasm32 has a fixed IEEE environment): [`FpEnvReport::supported`] is `false`
//!   and nothing is read or written.
//!
//! The environment is per thread: the guard affects only the calling thread.

/// What [`ensure_default_fp_env`] or [`read_fp_env`] found on the calling thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FpEnvReport {
    /// `false` on a target without a guard (nothing was read or written).
    pub supported: bool,
    /// Flush-to-zero was on (x86-64 MXCSR.FTZ, aarch64 FPCR.FZ or FZ16).
    pub flush_to_zero: bool,
    /// Denormals-are-zero was on (x86-64 MXCSR.DAZ; aarch64 has no separate bit).
    pub denormals_are_zero: bool,
    /// The rounding mode was not round-to-nearest-even.
    pub rounding_not_nearest: bool,
    /// A floating-point exception trap was enabled (x86-64: an exception mask was clear).
    pub traps_enabled: bool,
    /// aarch64 only: another non-default control bit was set (default NaN, alternative half
    /// precision, or the FEAT_AFP bits FIZ, AH and NEP).
    pub other_control: bool,
    /// The raw control register before the guard ran (MXCSR or FPCR); `0` when unsupported.
    pub raw_before: u64,
}

impl FpEnvReport {
    /// `true` when the environment was already the default (or the target has no guard).
    pub fn was_default(&self) -> bool {
        !(self.flush_to_zero
            || self.denormals_are_zero
            || self.rounding_not_nearest
            || self.traps_enabled
            || self.other_control)
    }
}

/// Read the calling thread's floating-point environment without changing it.
pub fn read_fp_env() -> FpEnvReport {
    imp::report(imp::read())
}

/// Put the calling thread's floating-point environment back to the IEEE default and report what it
/// was. Cheap (a register read, and a write only when something was wrong); call it at sim-thread
/// start and after foreign code ran on the sim thread.
pub fn ensure_default_fp_env() -> FpEnvReport {
    let raw = imp::read();
    let report = imp::report(raw);
    if !report.was_default() {
        imp::write_default(raw);
    }
    report
}

/// Where an [`FpEnvWatch`] check ran, relative to the sim step it brackets (ADR-8840).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpCheckPoint {
    /// Before the step of the reported tick. A change found here was made by code that ran on the
    /// thread between two steps (an audio, video or driver call on a shared thread); the guard put
    /// the default back before the step started, so the step ran under the IEEE default.
    BeforeStep,
    /// After the step of the reported tick. A change found here was made during that step (foreign
    /// code called from inside it); part of the step may already have run under the changed
    /// environment, so its results can differ from a peer's. The guard put the default back for
    /// the next step.
    AfterStep,
}

impl FpCheckPoint {
    /// Stable lowercase name for machine-readable reports: `before_step` or `after_step`.
    pub const fn code(self) -> &'static str {
        match self {
            Self::BeforeStep => "before_step",
            Self::AfterStep => "after_step",
        }
    }
}

/// One non-default environment an [`FpEnvWatch`] found and put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpEnvChange {
    /// The tick of the step the check bracketed.
    pub tick: u64,
    /// Whether the check ran before or after that step.
    pub point: FpCheckPoint,
    /// What [`ensure_default_fp_env`] found (its `raw_before` is the changed register).
    pub report: FpEnvReport,
}

/// The periodic floating-point environment check of a sim loop (ADR-8840).
///
/// [`ensure_default_fp_env`] at thread start is not enough when foreign code shares the sim thread:
/// a library that sets flush-to-zero between two steps (or from inside one) changes subnormal
/// results on that peer only, and the match drifts without an error. A loop owns one watch and
/// calls [`FpEnvWatch::before_step`] right before each step and [`FpEnvWatch::after_step`] right
/// after it: those are the defined points where the environment is read, and put back when it is
/// not the default. Each check is one control-register read (and a write only when something was
/// wrong), so checking every step is the default; [`FpEnvWatch::new`] takes a coarser cadence for a
/// loop that wants one. With a cadence above one step a change is found at the next checked step,
/// and [`FpEnvChange::point`] says where it was found rather than where it was made.
///
/// [`crate::harness::SimHarness`] runs one at every step; a host with its own loop (an external
/// game sim) owns one the same way. On a target without a guard every check is a no-op that still
/// counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpEnvWatch {
    every: u32,
    since: u32,
    due: bool,
    checks: u64,
    corrections: u64,
    during_step: u64,
    first: Option<FpEnvChange>,
    last: Option<FpEnvChange>,
}

impl Default for FpEnvWatch {
    /// A watch that checks around every step.
    fn default() -> Self {
        Self::new(Self::EVERY_STEP)
    }
}

impl FpEnvWatch {
    /// The default cadence: check before and after every step.
    pub const EVERY_STEP: u32 = 1;

    /// A watch that checks around one step in `every` (`1` every step, `0` never: the loop relies
    /// on its thread-start guard alone). The first step is always checked when `every > 0`.
    pub const fn new(every: u32) -> Self {
        Self {
            every,
            // The first `before_step` is due, whatever the cadence.
            since: every.saturating_sub(1),
            due: false,
            checks: 0,
            corrections: 0,
            during_step: 0,
            first: None,
            last: None,
        }
    }

    /// The cadence: steps between two checked steps (`0`: never checked).
    pub const fn every(&self) -> u32 {
        self.every
    }

    /// Call right before the step of `tick`. On a checked step, read the environment, put the
    /// default back if it changed, and return the change.
    pub fn before_step(&mut self, tick: u64) -> Option<FpEnvChange> {
        self.due = false;
        if self.every == 0 {
            return None;
        }
        self.since += 1;
        if self.since < self.every {
            return None;
        }
        self.since = 0;
        self.due = true;
        self.check(tick, FpCheckPoint::BeforeStep)
    }

    /// Call right after the step of `tick`. On a step that [`FpEnvWatch::before_step`] checked,
    /// read the environment again: a change here happened during the step.
    pub fn after_step(&mut self, tick: u64) -> Option<FpEnvChange> {
        if !std::mem::take(&mut self.due) {
            return None;
        }
        self.check(tick, FpCheckPoint::AfterStep)
    }

    /// Check now, whatever the cadence, and count it as a check at `point` of `tick`.
    pub fn check(&mut self, tick: u64, point: FpCheckPoint) -> Option<FpEnvChange> {
        let report = ensure_default_fp_env();
        self.checks += 1;
        if report.was_default() {
            return None;
        }
        let change = FpEnvChange { tick, point, report };
        self.corrections += 1;
        if point == FpCheckPoint::AfterStep {
            self.during_step += 1;
        }
        self.first.get_or_insert(change);
        self.last = Some(change);
        tracing::warn!(
            tick,
            point = point.code(),
            raw = report.raw_before,
            "the sim thread's floating-point environment changed; reset it to the IEEE default"
        );
        Some(change)
    }

    /// Checks run so far (before and after steps).
    pub const fn checks(&self) -> u64 {
        self.checks
    }

    /// Checks that found a non-default environment and put the default back.
    pub const fn corrections(&self) -> u64 {
        self.corrections
    }

    /// The corrections found after a step: the environment changed during that step, so the step
    /// may have run partly under it. Non-zero is a desync risk the host should report.
    pub const fn during_step(&self) -> u64 {
        self.during_step
    }

    /// The first change found, if any.
    pub const fn first_change(&self) -> Option<FpEnvChange> {
        self.first
    }

    /// The latest change found, if any.
    pub const fn last_change(&self) -> Option<FpEnvChange> {
        self.last
    }
}

/// Turn flush-to-zero on for the calling thread (x86-64: FTZ and DAZ; aarch64: FZ) and report what
/// was there before. Behind the `fp-test-hooks` feature, for tests that prove a guard runs: a thread
/// spawned after this call inherits the setting (POSIX threads inherit the floating-point
/// environment), and foreign code that sets FTZ on a sim thread is modelled by calling it there. Call
/// [`ensure_default_fp_env`] on the thread before any float code that matters runs. A no-op on a
/// target without a guard. This crate's own unit tests use it without the feature.
#[cfg(any(test, feature = "fp-test-hooks"))]
pub fn set_flush_to_zero_for_tests() -> FpEnvReport {
    let raw = imp::read();
    imp::write_flush_to_zero(raw);
    imp::report(raw)
}

// det-scan: allow(arch_cfg): register access restores the same IEEE default on each supported architecture
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)] // ADR-8013: MXCSR is only reachable through `stmxcsr`/`ldmxcsr` (docs/04 § 5).
mod imp {
    use super::FpEnvReport;

    /// The default MXCSR control field: every exception masked, round to nearest, no FTZ or DAZ.
    pub(super) const DEFAULT_CONTROL: u32 = 0x1F80;
    /// The control field of MXCSR (bits 6-15); bits 0-5 are the sticky status flags.
    const CONTROL_MASK: u32 = 0xFFC0;
    const FTZ: u32 = 1 << 15;
    const DAZ: u32 = 1 << 6;
    const ROUNDING: u32 = 0b11 << 13;
    const EXCEPTION_MASKS: u32 = 0b11_1111 << 7;

    pub(super) fn read() -> u64 {
        let mut csr: u32 = 0;
        // SAFETY: `stmxcsr` stores the 32-bit MXCSR to the pointed-to `u32`, which is valid, aligned
        // and writable for the duration of the block. It changes no register, flag or float state.
        unsafe {
            core::arch::asm!(
                "stmxcsr [{ptr}]",
                ptr = in(reg) &mut csr,
                options(nostack, preserves_flags)
            );
        }
        u64::from(csr)
    }

    pub(super) fn write_default(raw: u64) {
        let csr = (raw as u32 & !CONTROL_MASK) | DEFAULT_CONTROL;
        write(csr);
    }

    #[cfg(any(test, feature = "fp-test-hooks"))]
    pub(super) fn write_flush_to_zero(raw: u64) {
        write(raw as u32 | FTZ | DAZ);
    }

    /// Load `csr` into MXCSR.
    pub(super) fn write(csr: u32) {
        // SAFETY: `ldmxcsr` loads MXCSR from the pointed-to `u32`, which is valid and aligned. The
        // callers write either the default control field (which is the environment the compiler
        // assumes, so no float code is affected) or, in tests (this crate's, and behind the
        // `fp-test-hooks` feature), a planted FTZ value that the test resets before any float
        // operation that matters runs.
        unsafe {
            core::arch::asm!(
                "ldmxcsr [{ptr}]",
                ptr = in(reg) &csr,
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    pub(super) fn report(raw: u64) -> FpEnvReport {
        let csr = raw as u32;
        FpEnvReport {
            supported: true,
            flush_to_zero: csr & FTZ != 0,
            denormals_are_zero: csr & DAZ != 0,
            rounding_not_nearest: csr & ROUNDING != 0,
            traps_enabled: csr & EXCEPTION_MASKS != EXCEPTION_MASKS,
            other_control: false,
            raw_before: raw,
        }
    }
}

// det-scan: allow(arch_cfg): register access restores the same IEEE default on each supported architecture
#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)] // ADR-8013: FPCR is only reachable through `mrs`/`msr` (docs/04 § 5).
mod imp {
    use super::FpEnvReport;

    const FIZ: u64 = 1 << 0;
    const AH: u64 = 1 << 1;
    const NEP: u64 = 1 << 2;
    const TRAPS: u64 = (0b1_1111 << 8) | (1 << 15);
    const FZ16: u64 = 1 << 19;
    const ROUNDING: u64 = 0b11 << 22;
    const FZ: u64 = 1 << 24;
    const DN: u64 = 1 << 25;
    const AHP: u64 = 1 << 26;
    /// Every control bit the guard knows; all clear is the default on Linux, Android and macOS.
    const CONTROL: u64 = FIZ | AH | NEP | TRAPS | FZ16 | ROUNDING | FZ | DN | AHP;

    pub(super) fn read() -> u64 {
        let fpcr: u64;
        // SAFETY: `mrs` copies FPCR into a general register; it changes no memory, flag or float
        // state.
        unsafe {
            core::arch::asm!(
                "mrs {fpcr}, fpcr",
                fpcr = out(reg) fpcr,
                options(nomem, nostack, preserves_flags)
            );
        }
        fpcr
    }

    pub(super) fn write_default(raw: u64) {
        write(raw & !CONTROL);
    }

    #[cfg(any(test, feature = "fp-test-hooks"))]
    pub(super) fn write_flush_to_zero(raw: u64) {
        write(raw | FZ);
    }

    /// Load `fpcr` into FPCR.
    pub(super) fn write(fpcr: u64) {
        // SAFETY: `msr fpcr` sets the float control register from a general register; it touches no
        // memory. The callers write either the default control bits (the environment the compiler
        // assumes) or, in tests (this crate's, and behind the `fp-test-hooks` feature), a planted FZ
        // value that the test resets before any float operation that matters runs.
        unsafe {
            core::arch::asm!(
                "msr fpcr, {fpcr}",
                fpcr = in(reg) fpcr,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    pub(super) fn report(raw: u64) -> FpEnvReport {
        FpEnvReport {
            supported: true,
            flush_to_zero: raw & (FZ | FZ16) != 0,
            denormals_are_zero: false,
            rounding_not_nearest: raw & ROUNDING != 0,
            traps_enabled: raw & TRAPS != 0,
            other_control: raw & (DN | AHP | FIZ | AH | NEP) != 0,
            raw_before: raw,
        }
    }
}

// det-scan: allow(arch_cfg): unsupported targets report no guard; no alternative simulation arithmetic
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    use super::FpEnvReport;

    pub(super) fn read() -> u64 {
        0
    }

    pub(super) fn write_default(_raw: u64) {}

    #[cfg(any(test, feature = "fp-test-hooks"))]
    pub(super) fn write_flush_to_zero(_raw: u64) {}

    pub(super) fn report(_raw: u64) -> FpEnvReport {
        FpEnvReport::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    /// A subnormal product computed at run time: `0.0` on a thread that flushes to zero.
    fn subnormal_product() -> f32 {
        black_box(f32::MIN_POSITIVE) * black_box(0.5)
    }

    #[test]
    fn a_default_thread_reports_default_and_keeps_subnormals() {
        let report = ensure_default_fp_env();
        assert!(report.was_default(), "{report:?}");
        assert!(read_fp_env().was_default());
        let sub = subnormal_product();
        assert!(sub > 0.0 && sub.is_subnormal(), "{sub:e}");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn ftz_guard_resets_a_set_mxcsr() {
        let before = imp::read() as u32;
        assert_eq!(
            before & 0xFFC0,
            imp::DEFAULT_CONTROL,
            "the test thread starts at the default"
        );
        // FTZ, DAZ and round-toward-zero: what an audio library's worker thread can hand back. No
        // float operation runs between this write and the guard.
        imp::write(before | (1 << 15) | (1 << 6) | (0b11 << 13));

        let report = ensure_default_fp_env();
        let after = imp::read() as u32;
        assert!(report.supported);
        assert!(report.flush_to_zero, "{report:?}");
        assert!(report.denormals_are_zero, "{report:?}");
        assert!(report.rounding_not_nearest, "{report:?}");
        assert!(!report.was_default());
        assert_eq!(
            after & 0xFFC0,
            imp::DEFAULT_CONTROL,
            "MXCSR after the guard: {after:#x}"
        );
        assert!(read_fp_env().was_default());
        let sub = subnormal_product();
        assert!(sub > 0.0 && sub.is_subnormal(), "{sub:e}");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn ftz_guard_unmasks_nothing_and_keeps_status_flags() {
        let before = imp::read() as u32;
        // Unmask the precision exception (bit 12) and set the sticky inexact flag (bit 5).
        imp::write((before & !(1 << 12)) | (1 << 5));
        let report = ensure_default_fp_env();
        let after = imp::read() as u32;
        assert!(report.traps_enabled, "{report:?}");
        assert_eq!(after & 0xFFC0, imp::DEFAULT_CONTROL);
        assert_ne!(after & (1 << 5), 0, "the sticky status flags are kept");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn ftz_guard_resets_a_set_fpcr() {
        let before = imp::read();
        assert_eq!(
            before & ((1 << 24) | (1 << 25) | (0b11 << 22)),
            0,
            "the test thread starts at the default"
        );
        // FZ, DN and round-toward-zero. No float operation runs between this write and the guard.
        imp::write(before | (1 << 24) | (1 << 25) | (0b11 << 22));

        let report = ensure_default_fp_env();
        let after = imp::read();
        assert!(report.supported);
        assert!(report.flush_to_zero, "{report:?}");
        assert!(report.rounding_not_nearest, "{report:?}");
        assert!(report.other_control, "{report:?}");
        assert!(!report.was_default());
        assert_eq!(
            after & ((1 << 24) | (1 << 25) | (0b11 << 22)),
            0,
            "FPCR after the guard: {after:#x}"
        );
        let sub = subnormal_product();
        assert!(sub > 0.0 && sub.is_subnormal(), "{sub:e}");
    }

    /// ADR-8840: a cadence of one checks around every step, and a change planted between two
    /// steps is put back before the next one and named as `before_step`.
    #[test]
    fn watch_resets_a_change_made_between_steps() {
        let supported = read_fp_env().supported;
        let mut watch = FpEnvWatch::default();
        assert_eq!(watch.before_step(0), None);
        assert_eq!(watch.after_step(0), None);
        let planted = set_flush_to_zero_for_tests();
        assert!(planted.was_default(), "{planted:?}");
        let found = watch.before_step(1);
        let now = read_fp_env();
        // Put the thread back before any assertion can unwind with FTZ still set.
        ensure_default_fp_env();
        assert!(now.was_default(), "the watch put the default back: {now:?}");
        assert_eq!(watch.after_step(1), None);
        assert_eq!(watch.checks(), 4);
        assert_eq!(watch.corrections(), u64::from(supported));
        assert_eq!(watch.during_step(), 0);
        if supported {
            let change = found.expect("the planted FTZ is found");
            assert_eq!((change.tick, change.point), (1, FpCheckPoint::BeforeStep));
            assert!(change.report.flush_to_zero, "{change:?}");
            assert_eq!(watch.first_change(), Some(change));
            assert_eq!(watch.last_change(), Some(change));
            assert_eq!(change.point.code(), "before_step");
        }
    }

    /// ADR-8840: a change made inside a step is found by the check right after it and counted as
    /// a during-step change.
    #[test]
    fn watch_names_a_change_made_during_a_step() {
        let supported = read_fp_env().supported;
        let mut watch = FpEnvWatch::default();
        assert_eq!(watch.before_step(7), None);
        set_flush_to_zero_for_tests();
        let found = watch.after_step(7);
        let now = read_fp_env();
        ensure_default_fp_env();
        assert!(now.was_default(), "{now:?}");
        assert_eq!(watch.during_step(), u64::from(supported));
        if supported {
            let change = found.expect("found after the step");
            assert_eq!((change.tick, change.point), (7, FpCheckPoint::AfterStep));
            assert_eq!(change.point.code(), "after_step");
        }
        assert_eq!(watch.before_step(8), None, "the next step starts at the default");
    }

    /// ADR-8840: a coarser cadence checks the first step and then one step in `every`, before and
    /// after; `0` never checks.
    #[test]
    fn watch_cadence_counts_checked_steps_only() {
        let mut watch = FpEnvWatch::new(3);
        assert_eq!(watch.every(), 3);
        for tick in 0..7 {
            watch.before_step(tick);
            watch.after_step(tick);
        }
        // Ticks 0, 3 and 6 are checked, twice each.
        assert_eq!(watch.checks(), 6);
        let mut off = FpEnvWatch::new(0);
        for tick in 0..4 {
            assert_eq!(off.before_step(tick), None);
            assert_eq!(off.after_step(tick), None);
        }
        assert_eq!(off.checks(), 0);
        // An after_step without a checked before_step does not check.
        let mut watch = FpEnvWatch::default();
        assert_eq!(watch.after_step(0), None);
        assert_eq!(watch.checks(), 0);
        assert_eq!(watch.check(0, FpCheckPoint::AfterStep), None);
        assert_eq!(watch.checks(), 1);
    }
}
