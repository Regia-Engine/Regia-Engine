// SPDX-License-Identifier: MPL-2.0
//! Float-policy F4 (review sprint-v06-absorb-rev-a, report sprint-detsim-float-policy-ff): the golden
//! harness puts the recording thread's floating-point environment back to the IEEE default before a
//! scenario runs, so a test thread that inherited flush-to-zero (or set it) records the same trace
//! as a clean thread instead of a false divergence (or a false agreement).

use std::hint::black_box;

use super::*;
use crate::fpu;

/// Halves a subnormal-range value every tick and hashes it: under flush-to-zero the first product
/// below `f32::MIN_POSITIVE` reads `0.0`, so the trace differs from tick 0.
struct Subnormal {
    v: f32,
}

impl DetScenario for Subnormal {
    fn name(&self) -> &str {
        "fp_env_subnormal"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(1, 8, TickRate::DEFAULT)
    }

    fn reset(&mut self, _run: &DetRun) {
        self.v = f32::MIN_POSITIVE;
    }

    fn step(&mut self, _tick: u32) {
        self.v = black_box(self.v) * black_box(0.5);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.f32("v", self.v);
    }
}

#[test]
fn record_resets_a_flush_to_zero_thread_before_the_run() {
    let clean = record(&mut Subnormal { v: 0.0 }, &DetRun::new(1, 8, TickRate::DEFAULT));
    let planted = fpu::set_flush_to_zero_for_tests();
    assert!(
        planted.was_default(),
        "the test thread started clean: {planted:?}"
    );
    let supported = fpu::read_fp_env().supported;
    let dirty = record(&mut Subnormal { v: 0.0 }, &DetRun::new(1, 8, TickRate::DEFAULT));
    let after = fpu::ensure_default_fp_env();
    assert_eq!(
        dirty.tick_hashes, clean.tick_hashes,
        "a recording on a flush-to-zero thread must equal a clean one (the harness resets the environment)"
    );
    if supported {
        assert!(
            after.was_default(),
            "the environment stays default after the run: {after:?}"
        );
    }
}

#[test]
fn check_golden_names_a_reset_environment_in_its_detail() {
    let dir = std::env::temp_dir().join(format!("regia-fp-env-golden-{}", std::process::id()));
    let mut scenario = Subnormal { v: 0.0 };
    let _ = check_golden(&mut scenario, &dir, GoldenMode::Bless);
    fpu::set_flush_to_zero_for_tests();
    let supported = fpu::read_fp_env().supported;
    let report = check_golden(&mut scenario, &dir, GoldenMode::Check);
    fpu::ensure_default_fp_env();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(report.passed(), "{report}");
    if supported {
        assert!(
            report.detail.contains("fp environment"),
            "the report names the reset: {}",
            report.detail
        );
    }
}
