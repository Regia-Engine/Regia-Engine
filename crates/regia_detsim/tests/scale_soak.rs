// SPDX-License-Identifier: MPL-2.0
//! F-07: repeatable 40-seat, 900-entity load, checked every tick.
use regia_detsim::golden::{self, DetScenario};
use regia_detsim::scale_soak::{ScaleScenario, SoakConfig};

#[test]
fn scale_soak_900_entities_hash_is_stable() {
    let config = SoakConfig::default();
    let mut a = ScaleScenario::new(config).expect("valid reference settings");
    let mut b = ScaleScenario::new(config).expect("valid reference settings");
    let run = a.default_run();
    let trace_a = golden::record(&mut a, &run);
    let trace_b = golden::record(&mut b, &run);
    assert_eq!(
        golden::first_divergence(&trace_a.tick_hashes, &trace_b.tick_hashes),
        None
    );
    assert_eq!(trace_a.run_hash(), trace_b.run_hash());
    assert_eq!(a.entity_count(), 900);
    assert_eq!(a.seat_count(), 40);
    assert!(a.contacts() > 1_000, "must exercise sustained contact work");
    assert!(a.recycled() >= 40, "must exercise generation reuse");
}

#[test]
fn scale_soak_matches_all_pairs_every_tick_and_reset() {
    let config = SoakConfig::default();
    let mut grid = ScaleScenario::new(config).expect("valid config");
    let mut reference = ScaleScenario::new(config).expect("valid config");
    let run = grid.default_run();
    grid.reset(&run);
    reference.reset(&run);
    let initial = grid.fingerprint();
    for tick in 0..180 {
        grid.step(tick);
        reference.step_reference(tick);
        assert_eq!(grid.fingerprint(), reference.fingerprint(), "tick {tick}");
        assert_eq!(grid.bodies(), reference.bodies(), "tick {tick}");
        assert!(grid.bodies().iter().all(|b| b.pos.iter().all(|v| v.is_finite())));
    }
    assert!(grid.recycled() >= 120);
    grid.reset(&run);
    assert_eq!(grid.fingerprint(), initial, "reset clears generations and caches");
}

#[test]
fn all_seats_drive_entities_and_seed_changes_trace() {
    let mut a = ScaleScenario::new(SoakConfig::default()).expect("valid config");
    let mut run = a.default_run();
    run.ticks = 3;
    let first = golden::record(&mut a, &run);
    let mut fields = golden::StateHasher::with_log();
    a.hash_state(&mut fields);
    for seat in 0..40 {
        assert!(
            fields
                .fields()
                .iter()
                .any(|v| v.name == format!("seat[{seat}].input_x"))
        );
        assert!(
            fields
                .fields()
                .iter()
                .any(|v| v.name.ends_with(".seat") && v.bits == seat)
        );
    }
    run.seed ^= 1;
    let second = golden::record(&mut a, &run);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &second.tick_hashes),
        Some(0)
    );
}

#[test]
fn scale_soak_flip_names_the_first_field_and_tick() {
    let config = SoakConfig {
        seats: 4,
        entities: 27,
        ..SoakConfig::default()
    };
    let mut a = ScaleScenario::new(config).expect("valid config");
    let mut b = ScaleScenario::new(config).expect("valid config");
    let run = golden::DetRun {
        ticks: 30,
        ..a.default_run()
    };
    let difference = golden::compare(&mut a, &mut b, &run, Some(golden::Injection { tick: 17, bit: 3 }))
        .expect("supports injection")
        .expect("flipped bit diverges");
    assert_eq!(difference.tick, 17);
    assert!(difference.fields.iter().any(|field| field.name == "tick"));
}

#[test]
fn configured_counts_include_minimum_uneven_and_wrapping_recycle_batches() {
    for (seats, entities) in [(1, 1), (7, 7), (7, 19), (40, 901)] {
        let config = SoakConfig {
            seats,
            entities,
            recycle_interval: 1,
            ..SoakConfig::default()
        };
        let mut a = ScaleScenario::new(config).expect("valid config");
        let mut b = ScaleScenario::new(config).expect("valid config");
        for tick in 0..23 {
            a.step(tick);
            b.step_reference(tick);
            assert_eq!(
                a.fingerprint(),
                b.fingerprint(),
                "{seats}/{entities}, tick {tick}"
            );
            assert_eq!(a.entity_count(), usize::from(entities));
        }
        assert_eq!(a.recycled(), u64::from(seats) * 23);
        let mut fields = golden::StateHasher::with_log();
        a.hash_state(&mut fields);
        let generations: Vec<_> = fields
            .fields()
            .iter()
            .filter(|v| v.name.ends_with(".generation"))
            .collect();
        assert_eq!(generations.len(), usize::from(entities));
        assert_eq!(generations.iter().map(|v| v.bits).sum::<u64>(), a.recycled());
    }
}

#[test]
fn invalid_settings_are_rejected_before_stepping() {
    let default = SoakConfig::default();
    for config in [
        SoakConfig { seats: 0, ..default },
        SoakConfig {
            entities: 0,
            ..default
        },
        SoakConfig {
            seats: 901,
            ..default
        },
        SoakConfig {
            half_extent: f32::NAN,
            ..default
        },
        SoakConfig {
            half_extent: f32::INFINITY,
            ..default
        },
        SoakConfig {
            half_extent: 0.0,
            ..default
        },
        SoakConfig {
            radius: 0.0,
            ..default
        },
        SoakConfig {
            radius: 64.0,
            ..default
        },
        SoakConfig {
            radius: f32::NAN,
            ..default
        },
        SoakConfig {
            speed: -1.0,
            ..default
        },
        SoakConfig {
            speed: f32::INFINITY,
            ..default
        },
        SoakConfig {
            recycle_interval: 0,
            ..default
        },
    ] {
        let error = config.validate().expect_err("invalid");
        assert_eq!(error.code(), "invalid_soak_config");
        assert!(ScaleScenario::new(config).is_err());
    }
}

#[test]
fn percentile_and_budget_flags_do_not_hide_a_single_slow_pass() {
    use regia_detsim::scale_soak::profile::PassSummary;
    assert!(PassSummary::from_samples(&[], 100_000).is_none());
    let only = PassSummary::from_samples(&[100_000], 100_000).expect("one");
    assert_eq!(
        (only.p50_ns, only.p99_ns, only.max_ns),
        (100_000, 100_000, 100_000)
    );
    assert!(only.over_budget.is_empty(), "equal to budget is not over");
    let mut samples: Vec<u64> = (1..=100).rev().collect();
    samples[51] = 100_001;
    let summary = PassSummary::from_samples(&samples, 100_000).expect("samples");
    assert_eq!(summary.p50_ns, 51);
    assert_eq!(summary.p99_ns, 100);
    assert_eq!(summary.max_ns, 100_001);
    assert_eq!(summary.over_budget, [51]);
    let odd = PassSummary::from_samples(&[9, 2, 1], 1).expect("odd count");
    assert_eq!((odd.p50_ns, odd.p99_ns), (2, 9));
    assert_eq!(odd.over_budget, [0, 1]);
}

#[test]
fn observed_step_has_balanced_passes_and_identical_state() {
    use regia_detsim::scale_soak::{SoakObserver, SoakPass};
    struct Observer(Vec<(SoakPass, bool)>);
    impl SoakObserver for Observer {
        fn begin(&mut self, pass: SoakPass) {
            self.0.push((pass, true));
        }
        fn end(&mut self, pass: SoakPass) {
            self.0.push((pass, false));
        }
    }
    let mut a = ScaleScenario::new(SoakConfig::default()).expect("valid config");
    let mut b = ScaleScenario::new(SoakConfig::default()).expect("valid config");
    let mut observer = Observer(Vec::new());
    a.step_observed(0, &mut observer);
    b.step(0);
    assert_eq!(a.fingerprint(), b.fingerprint());
    let mut expected = vec![(SoakPass::Step, true)];
    for pass in SoakPass::ALL.into_iter().take(4) {
        expected.extend([(pass, true), (pass, false)]);
    }
    expected.push((SoakPass::Step, false));
    assert_eq!(observer.0, expected);
}

#[test]
fn measurement_excludes_warmup_and_preserves_the_full_trace() {
    use regia_detsim::scale_soak::profile;
    let config = SoakConfig {
        seats: 3,
        entities: 17,
        ..SoakConfig::default()
    };
    let mut a = ScaleScenario::new(config).expect("valid config");
    let run = golden::DetRun {
        ticks: 5,
        ..a.default_run()
    };
    let report = profile::measure(&mut a, run, 3, u64::MAX).expect("valid range");
    assert_eq!(report.run.ticks, 8);
    assert_eq!(report.trace.tick_hashes.len(), 8);
    assert!(report.passes.iter().all(|p| p.samples == 5));
    assert!(
        !report.over_budget(),
        "u64::MAX accepts any representable duration"
    );
    let mut b = ScaleScenario::new(config).expect("valid config");
    let trace = golden::record(&mut b, &report.run);
    assert_eq!(report.trace.tick_hashes, trace.tick_hashes);
    let json = report.to_json();
    assert!(json.contains("\"warmup\":3"));
    assert!(json.contains("\"ticks\":5"));
    assert!(json.contains("\"status\":\"ok\""));
    assert_eq!(json.matches("\"samples\":5").count(), 5);
    assert!(profile::measure(&mut a, golden::DetRun { ticks: 0, ..run }, 0, 1).is_err());
    assert!(profile::measure(&mut a, run, u32::MAX, 1).is_err());
}

#[test]
fn injection_changes_only_the_first_hashed_field_and_rejects_invalid_bits() {
    let mut scenario = ScaleScenario::new(SoakConfig::default()).expect("valid config");
    let mut before = golden::StateHasher::with_log();
    scenario.hash_state(&mut before);
    assert!(scenario.flip_bit(3));
    let mut after = golden::StateHasher::with_log();
    scenario.hash_state(&mut after);
    assert_eq!(before.fields()[0].name, "tick");
    assert_eq!(before.fields()[0].bits ^ after.fields()[0].bits, 1 << 3);
    assert_eq!(&before.fields()[1..], &after.fields()[1..]);
    let fingerprint = scenario.fingerprint();
    assert!(!scenario.flip_bit(32));
    assert_eq!(scenario.fingerprint(), fingerprint);
}

// ---------------------------------------------------------------------------------------------
// AI-on soak (ADR-8561): teams, range queries, shots, bursts, recycling and contacts.
// ---------------------------------------------------------------------------------------------

#[path = "support/ai_soak_pin.rs"]
mod ai_soak_pin;

mod ai_soak {
    use std::sync::atomic::{AtomicU64, Ordering};

    use regia_detsim::golden::{self, DetScenario};
    use regia_detsim::profile::{ClockKind, ProfileClock};
    use regia_detsim::scale_soak::ai::{
        AI_SOAK_SYSTEMS, AiQuery, AiSoakConfig, AiSoakConfigError, AiSoakObserver, AiSoakRunner,
        AiSoakScenario, AiSystem,
    };

    use super::ai_soak_pin::AI_SOAK_900_RUN_HASH;

    #[test]
    fn ai_soak_900_hash_is_stable() {
        let config = AiSoakConfig::default();
        let mut a = AiSoakScenario::new(config).expect("default settings are valid");
        let mut b = AiSoakScenario::new(config).expect("default settings are valid");
        let run = a.default_run();
        assert_eq!(run.ticks, 600);
        let trace_a = golden::record(&mut a, &run);
        let trace_b = golden::record(&mut b, &run);
        assert_eq!(
            golden::first_divergence(&trace_a.tick_hashes, &trace_b.tick_hashes),
            None
        );
        println!("ai_soak run_hash {:016x}", trace_a.run_hash());
        assert_eq!(
            trace_a.run_hash(),
            AI_SOAK_900_RUN_HASH,
            "pinned fingerprint moved: {:016x}",
            trace_a.run_hash()
        );
        assert_eq!(a.entity_count(), 900);
        assert_eq!(a.seat_count(), 40);
        assert_eq!(a.team_count(), 4);
        let k = a.counters();
        println!("ai_soak counters {k:?}");
        // The workload actually runs every query kind, fires, hits, recycles and collides.
        assert!(
            k.acquired > 0 && k.targets > 0 && k.allies > 0 && k.keep_out > 0,
            "{k:?}"
        );
        assert!(
            k.shots_fired > 0 && k.sweeps > k.shots_fired && k.hits > 0,
            "{k:?}"
        );
        assert!(k.burst_hits > 0 && k.recycled > 0 && k.contacts > 0, "{k:?}");
    }

    #[test]
    fn ai_soak_queries_match_the_linear_scan_every_tick() {
        let config = AiSoakConfig::default();
        let mut checked = AiSoakScenario::new(config).expect("valid");
        let mut plain = AiSoakScenario::new(config).expect("valid");
        let run = checked.default_run();
        checked.set_oracle(true);
        checked.reset(&run);
        plain.reset(&run);
        for tick in 0..120 {
            checked.step(tick);
            plain.step(tick);
            let oracle = checked.oracle().expect("oracle on");
            assert!(oracle.passed(), "tick {tick}: {:?}", oracle.first);
            // The oracle reads the state; it never changes it.
            assert_eq!(checked.fingerprint(), plain.fingerprint(), "tick {tick}");
        }
        let oracle = checked.oracle().expect("oracle on");
        for (kind, count) in [
            AiQuery::Acquire,
            AiQuery::Target,
            AiQuery::Ally,
            AiQuery::KeepOut,
            AiQuery::Segment,
            AiQuery::Burst,
        ]
        .into_iter()
        .zip(oracle.checked)
        {
            assert!(
                count > 0,
                "{} was never compared: {:?}",
                kind.name(),
                oracle.checked
            );
        }
        // Units: four per-unit queries every tick.
        assert_eq!(oracle.checked[AiQuery::Acquire as usize], 900 * 120);
        assert_eq!(oracle.checked[AiQuery::Ally as usize], 900 * 120);
        checked.set_oracle(false);
        assert!(checked.oracle().is_none());
    }

    #[test]
    fn ai_soak_counts_are_configurable() {
        let default = AiSoakConfig::default();
        for (teams, seats, entities, shots_per_tick) in [
            (2, 1, 2, 1),
            (3, 7, 19, 0),
            (7, 40, 300, 8),
            (64, 40, 200, 64),
            (4, 40, 1500, 48),
        ] {
            let config = AiSoakConfig {
                teams,
                seats,
                entities,
                shots_per_tick,
                max_shots: 512,
                half_extent: 24.0 + f32::from(entities) * 0.05,
                ..default
            };
            let mut scenario = AiSoakScenario::new(config).expect("valid counts");
            scenario.set_oracle(true);
            let run = golden::DetRun {
                ticks: 40,
                ..scenario.default_run()
            };
            let trace = golden::record(&mut scenario, &run);
            assert_eq!(trace.tick_hashes.len(), 40);
            assert_eq!(scenario.entity_count(), usize::from(entities));
            assert_eq!(scenario.seat_count(), usize::from(seats));
            assert_eq!(scenario.team_count(), usize::from(teams));
            let oracle = scenario.oracle().expect("oracle on");
            assert!(oracle.passed(), "{teams}/{seats}/{entities}: {:?}", oracle.first);
            let k = scenario.counters();
            if shots_per_tick == 0 {
                assert_eq!((k.shots_fired, k.sweeps), (0, 0), "firing off");
            } else {
                assert!(k.shots_fired <= u64::from(shots_per_tick) * 40, "{k:?}");
            }
            assert!(scenario.live_shots() <= 512);
            let mut fields = golden::StateHasher::with_log();
            scenario.hash_state(&mut fields);
            let teams_seen: std::collections::BTreeSet<_> = fields
                .fields()
                .iter()
                .filter(|v| v.name.starts_with("unit[") && v.name.ends_with(".team"))
                .map(|v| v.bits)
                .collect();
            assert_eq!(teams_seen.len(), usize::from(teams));
        }

        use AiSoakConfigError as E;
        for (config, error) in [
            (AiSoakConfig { teams: 1, ..default }, E::Teams),
            (AiSoakConfig { teams: 65, ..default }, E::Teams),
            (
                AiSoakConfig {
                    teams: 5,
                    seats: 4,
                    entities: 4,
                    ..default
                },
                E::Teams,
            ),
            (AiSoakConfig { seats: 0, ..default }, E::Counts),
            (
                AiSoakConfig {
                    seats: 901,
                    ..default
                },
                E::Counts,
            ),
            (
                AiSoakConfig {
                    half_extent: f32::NAN,
                    ..default
                },
                E::Geometry,
            ),
            (
                AiSoakConfig {
                    radius: 96.0,
                    ..default
                },
                E::Geometry,
            ),
            (
                AiSoakConfig {
                    speed: -1.0,
                    ..default
                },
                E::Geometry,
            ),
            (
                AiSoakConfig {
                    acquire_radius: 0.0,
                    ..default
                },
                E::Queries,
            ),
            (
                AiSoakConfig {
                    engage_radius: f32::INFINITY,
                    ..default
                },
                E::Queries,
            ),
            (
                AiSoakConfig {
                    acquire_k: 0,
                    ..default
                },
                E::Queries,
            ),
            (
                AiSoakConfig {
                    query_cell: -4.0,
                    ..default
                },
                E::Queries,
            ),
            (
                AiSoakConfig {
                    shots_per_tick: 600,
                    ..default
                },
                E::Shots,
            ),
            (
                AiSoakConfig {
                    shot_speed: 0.0,
                    ..default
                },
                E::Shots,
            ),
            (
                AiSoakConfig {
                    shot_life_ticks: 0,
                    ..default
                },
                E::Shots,
            ),
            (
                AiSoakConfig {
                    burst_radius: f32::NAN,
                    ..default
                },
                E::Shots,
            ),
            (AiSoakConfig { health: 0, ..default }, E::Combat),
            (
                AiSoakConfig {
                    cooldown_ticks: 0,
                    ..default
                },
                E::Combat,
            ),
        ] {
            assert_eq!(config.validate(), Err(error), "{config:?}");
            assert!(AiSoakScenario::new(config).is_err());
            let code = error.code();
            assert!(code.starts_with("invalid_ai_soak_"));
            assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert!(error.to_string().starts_with(code));
        }
        assert_eq!(default.validate(), Ok(()));
    }

    /// A clock that advances 1,000 ns on every read: fixed, deterministic "timings".
    #[derive(Default)]
    struct StepClock(AtomicU64);
    impl ProfileClock for StepClock {
        fn kind(&self) -> ClockKind {
            ClockKind::ThreadCpu
        }
        fn now_ns(&self) -> u64 {
            self.0.fetch_add(1_000, Ordering::Relaxed)
        }
    }

    #[test]
    fn ai_soak_profile_has_a_row_per_system() {
        let names: Vec<&str> = AiSystem::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names.as_slice(), AI_SOAK_SYSTEMS.names().as_slice());
        let config = AiSoakConfig {
            entities: 120,
            ..AiSoakConfig::default()
        };
        let scenario = AiSoakScenario::new(config).expect("valid");
        let run = golden::DetRun {
            ticks: 30,
            ..scenario.default_run()
        };
        let runner =
            AiSoakRunner::with_clock(scenario, run, 5, 15_000, StepClock::default()).expect("valid range");
        let measured = runner.finish();
        assert_eq!(measured.trace.tick_hashes.len(), 35);
        let report = &measured.report;
        assert_eq!(report.samples, 30);
        assert_eq!(report.clock, ClockKind::ThreadCpu);
        let rows: Vec<&str> = report.systems.iter().map(|r| r.system.as_str()).collect();
        assert_eq!(rows, names, "one row per system, in run order");
        for row in &report.systems {
            // begin and end are one read apart.
            assert_eq!(
                (row.p50_ns, row.p99_ns, row.max_ns),
                (1_000, 1_000, 1_000),
                "{row:?}"
            );
            assert_eq!(row.budget_ns, 15_000);
        }
        // begin_tick, two reads per system, end_tick: 20 reads after the origin.
        assert_eq!(report.tick.p99_ns, 21_000);
        assert!(!measured.inside_pass_line());
        assert_eq!(report.over_budget.len(), 30);
        assert_eq!(report.over_budget[0].tick, 5, "absolute ticks, after warmup");
        assert_eq!(report.over_budget[0].systems.len(), 10);
        // The JSON `sim.tick_profile` returns has the same ten rows.
        let json = serde_json::to_value(report).expect("serializes");
        assert_eq!(json["systems"].as_array().map(Vec::len), Some(10));
        assert_eq!(json["systems"][5]["system"], "keep_out");
        assert_eq!(json["clock"], "thread_cpu");

        // The same deterministic trace as an unprofiled recording.
        let mut plain = AiSoakScenario::new(config).expect("valid");
        let trace = golden::record(&mut plain, &measured.trace.run);
        assert_eq!(trace.tick_hashes, measured.trace.tick_hashes);

        // Balanced, ordered system events.
        struct Events(Vec<(AiSystem, bool)>, u32);
        impl AiSoakObserver for Events {
            fn begin_tick(&mut self) {
                self.1 += 1;
            }
            fn begin(&mut self, system: AiSystem) {
                self.0.push((system, true));
            }
            fn end(&mut self, system: AiSystem) {
                self.0.push((system, false));
            }
        }
        let mut events = Events(Vec::new(), 0);
        let mut observed = AiSoakScenario::new(config).expect("valid");
        observed.step_observed(0, &mut events);
        let expected: Vec<_> = AiSystem::ALL
            .into_iter()
            .flat_map(|s| [(s, true), (s, false)])
            .collect();
        assert_eq!(events.0, expected);
        assert_eq!(events.1, 1);

        assert_eq!(
            AiSoakRunner::with_clock(
                AiSoakScenario::new(config).expect("valid"),
                golden::DetRun { ticks: 0, ..run },
                0,
                1,
                StepClock::default()
            )
            .err()
            .map(|e| e.code()),
            Some("invalid_ai_soak_measurement")
        );
        assert!(
            AiSoakRunner::with_clock(
                AiSoakScenario::new(config).expect("valid"),
                run,
                u32::MAX,
                1,
                StepClock::default()
            )
            .is_err()
        );
    }

    #[test]
    fn ai_soak_flip_names_the_tick_field() {
        let config = AiSoakConfig {
            entities: 60,
            ..AiSoakConfig::default()
        };
        let mut a = AiSoakScenario::new(config).expect("valid");
        let mut b = AiSoakScenario::new(config).expect("valid");
        let run = golden::DetRun {
            ticks: 20,
            ..a.default_run()
        };
        let difference = golden::compare(&mut a, &mut b, &run, Some(golden::Injection { tick: 11, bit: 2 }))
            .expect("supports injection")
            .expect("flipped bit diverges");
        assert_eq!(difference.tick, 11);
        assert!(difference.fields.iter().any(|field| field.name == "tick"));
        assert!(!a.flip_bit(32));
    }
}
