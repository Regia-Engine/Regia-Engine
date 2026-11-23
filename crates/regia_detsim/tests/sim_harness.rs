// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/harness.rs, commit d6ed37677), ADR-8012.
// Adapt: the seed-100, 50-tick equality acceptance uses F-02's engine scenario;
// game bootstrap and game-specific snapshot fields remain host-owned (ADR-8151).
//! Seeded harness acceptance, bounded orchestration and AI arena integration.

use regia_detsim::golden::{DetRun, TickRate, scenarios::SteerScenario};
use regia_detsim::harness::SimHarness;

#[test]
fn same_seed_same_snapshot_after_n_ticks() {
    let run = DetRun::new(100, 50, TickRate::DEFAULT);
    let mut a = SimHarness::new(SteerScenario::new(), run).expect("scenario");
    let mut b = SimHarness::new(SteerScenario::new(), run).expect("scenario");
    a.step_n(50).expect("tick budget");
    b.step_n(50).expect("tick budget");
    assert_eq!(
        a.snapshot(),
        b.snapshot(),
        "identical seeds must produce identical snapshots at tick 50"
    );
}

use regia_detsim::arena::{ArenaScenario, ArenaSettings};
use regia_detsim::golden::{self, DetScenario, StateHasher};
use regia_detsim::harness::HarnessError;
use regia_detsim::{profile, soak};

#[test]
fn harness_matches_f02_at_every_tick_and_rejects_partial_batches() {
    let run = DetRun::new(7, 30, TickRate::DEFAULT);
    let trace = golden::record(&mut SteerScenario::new(), &run);
    let mut harness = SimHarness::new(SteerScenario::new(), run).expect("scenario");
    assert_eq!(harness.seed(), 7);
    assert_eq!(harness.tick(), 0);
    assert_eq!(harness.run(), run);
    let before = harness.snapshot();
    assert_eq!(harness.step_n(31), Err(HarnessError::TickBudget));
    assert_eq!(harness.snapshot(), before);
    for hash in trace.tick_hashes {
        harness.step().expect("remaining step");
        assert_eq!(harness.snapshot().fingerprint, hash);
    }
    assert_eq!(harness.remaining(), 0);
    assert_eq!(harness.step(), Err(HarnessError::TickBudget));
    harness.step_n(0).expect("no-op at end");
    assert_eq!(harness.tick(), 30);
}

#[test]
fn zero_ticks_capture_bootstrap_and_seeds_change_it() {
    let a = SimHarness::new(
        ArenaScenario::new(ArenaSettings::default()).expect("settings"),
        DetRun::new(1, 0, TickRate::DEFAULT),
    )
    .expect("scenario");
    let b = SimHarness::new(
        ArenaScenario::new(ArenaSettings::default()).expect("settings"),
        DetRun::new(2, 0, TickRate::DEFAULT),
    )
    .expect("scenario");
    assert_eq!(a.tick(), 0);
    assert_ne!(a.snapshot().fingerprint, b.snapshot().fingerprint);
    let report = soak::run(
        || ArenaScenario::new(ArenaSettings::default()).expect("settings"),
        a.run(),
        3,
    )
    .expect("soak");
    assert!(report.passed());
    assert_eq!(report.steps_compared, 0);
    assert_eq!(report.pairs_completed, 3);
    assert_eq!(report.final_snapshot, a.snapshot());
}

#[derive(Default)]
struct Probe {
    value: u32,
    inject_boundary: Option<u32>,
    bad_name: bool,
}

impl DetScenario for Probe {
    fn name(&self) -> &str {
        if self.bad_name { "bad name" } else { "probe" }
    }
    fn default_run(&self) -> DetRun {
        DetRun::new(0, 10, TickRate::DEFAULT)
    }
    fn reset(&mut self, _run: &DetRun) {
        self.value = u32::from(self.inject_boundary == Some(0));
    }
    fn step(&mut self, tick: u32) {
        // A transient flip disappears on the following tick.
        self.value = tick + 1 + u32::from(self.inject_boundary == Some(tick + 1));
    }
    fn hash_state(&self, h: &mut StateHasher) {
        h.u32("value", self.value);
    }
}

#[test]
fn soak_names_bootstrap_and_transient_first_divergence() {
    for boundary in [0, 1, 7, 10] {
        let mut calls = 0u32;
        let report = soak::run(
            || {
                calls += 1;
                Probe {
                    inject_boundary: (calls == 4).then_some(boundary),
                    ..Probe::default()
                }
            },
            Probe::default().default_run(),
            4,
        )
        .expect("soak");
        assert!(!report.passed());
        assert_eq!(calls, 4, "must stop at first failed pair");
        assert_eq!(report.pairs_completed, 1);
        assert_eq!(report.steps_compared, 10 + u64::from(boundary));
        let divergence = report.divergence.expect("injected mismatch");
        assert_eq!(divergence.pair, 1);
        assert_eq!(divergence.expected.tick, boundary);
        assert_eq!(divergence.actual.tick, boundary);
        assert_ne!(divergence.expected.fingerprint, divergence.actual.fingerprint);
    }
}

#[test]
fn invalid_requests_do_not_call_the_factory_or_bootstrap() {
    assert_eq!(
        soak::run::<Probe>(
            || panic!("empty soak must not bootstrap"),
            Probe::default().default_run(),
            0
        )
        .map(|_: soak::SoakReport| ()),
        Err(HarnessError::EmptySoak)
    );
    let invalid = SimHarness::new(
        Probe {
            bad_name: true,
            ..Probe::default()
        },
        Probe::default().default_run(),
    );
    assert_eq!(invalid.err().expect("invalid name").code(), "bad_scenario_name");
    let config = profile::ProfileConfig {
        seed: 7,
        rate: TickRate::DEFAULT,
        warmup_ticks: 10,
        samples: 0,
    };
    assert_eq!(
        profile::run(Probe::default(), config),
        Err(HarnessError::EmptyProfile)
    );
    assert_eq!(
        profile::run(
            Probe::default(),
            profile::ProfileConfig {
                samples: u32::MAX,
                ..config
            }
        ),
        Err(HarnessError::TickOverflow)
    );
}

#[test]
fn profile_steps_exactly_warmup_plus_samples_and_preserves_the_hash() {
    let config = profile::ProfileConfig {
        seed: 88,
        rate: TickRate::new(120).expect("rate"),
        warmup_ticks: 13,
        samples: 21,
    };
    let profile = profile::run(SteerScenario::new(), config).expect("profile");
    let mut harness =
        SimHarness::new(SteerScenario::new(), DetRun::new(88, 34, config.rate)).expect("scenario");
    harness.step_n(34).expect("steps");
    assert_eq!(profile.samples, 21);
    assert_eq!(profile.final_snapshot, harness.snapshot());
    // Structural ordering only, never a wall-clock performance budget.
    assert!(profile.p50_ns <= profile.p99_ns);
    assert!(profile.p99_ns <= profile.max_ns);
}

#[test]
fn ai_arena_repeats_and_reuses_f02_without_a_game_or_assets() {
    let settings = ArenaSettings::default();
    let run = DetRun::new(100, 1200, TickRate::DEFAULT);
    let create = || ArenaScenario::new(settings).expect("settings");
    let report = soak::run(create, run, 4).expect("soak");
    assert!(report.passed());
    assert_eq!(report.pairs_completed, 4);
    assert_eq!(report.steps_compared, 4800);
    let mut arena = create();
    let trace = golden::record(&mut arena, &run);
    assert_eq!(
        trace.tick_hashes.last().copied(),
        Some(report.final_snapshot.fingerprint)
    );
    let stats = arena.stats();
    assert!(stats.attacks > 0, "must exercise combat, not idle ticks");
    assert!(
        stats.alive.iter().sum::<u32>() < 8,
        "combat must remove living units"
    );
    assert!(stats.health.iter().sum::<u64>() < 800);
    let again = golden::record(&mut arena, &run);
    assert_eq!(trace, again, "reset must clear all prior match state");
}

#[test]
fn arena_simultaneous_lethal_attacks_have_no_first_team_advantage() {
    let settings = ArenaSettings {
        units_per_team: 1,
        spawn_radius: 1.0,
        attack_range: 100.0,
        damage: 100,
        ..ArenaSettings::default()
    };
    let mut harness = SimHarness::new(
        ArenaScenario::new(settings).expect("settings"),
        DetRun::new(9, 2, TickRate::DEFAULT),
    )
    .expect("scenario");
    harness.step().expect("step");
    assert_eq!(harness.state().stats().alive, [0, 0]);
    assert_eq!(harness.state().stats().attacks, 2);
    harness.step().expect("step after elimination");
    assert_eq!(harness.state().stats().attacks, 2);
}

#[test]
fn arena_cooldown_and_caps_are_settings_not_game_constants() {
    let settings = ArenaSettings {
        units_per_team: 3,
        spawn_radius: 1.0,
        attack_range: 100.0,
        damage: 1,
        health: 1000,
        cooldown_ticks: 3,
        ..ArenaSettings::default()
    };
    let mut harness = SimHarness::new(
        ArenaScenario::new(settings).expect("settings"),
        DetRun::new(9, 4, TickRate::DEFAULT),
    )
    .expect("scenario");
    harness.step_n(3).expect("three steps");
    assert_eq!(harness.state().stats().attacks, 6);
    harness.step().expect("fourth step");
    assert_eq!(harness.state().stats().attacks, 12);
    assert_eq!(harness.state().stats().alive, [3, 3]);
}

#[test]
fn arena_rejects_nonfinite_or_invalid_rules() {
    for value in [f32::NAN, f32::INFINITY, -1.0, 0.0, f32::MAX] {
        assert!(
            ArenaScenario::new(ArenaSettings {
                attack_range: value,
                ..ArenaSettings::default()
            })
            .is_err()
        );
    }
    assert!(
        ArenaScenario::new(ArenaSettings {
            units_per_team: 0,
            ..ArenaSettings::default()
        })
        .is_err()
    );
    assert!(
        ArenaScenario::new(ArenaSettings {
            speed: 0.0,
            ..ArenaSettings::default()
        })
        .is_ok()
    );
}
