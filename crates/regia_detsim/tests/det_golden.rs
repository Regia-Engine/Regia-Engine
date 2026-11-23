// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/rate_gate.rs, commit d6ed37677), ADR-8012.
// Mark: as-is for `rate_60_vs_120_position_heading_and_flat_shot` (gates, inputs, durations and messages).
// Named changes: it drives the engine's reference vehicle through `RateModel` instead of the game's hover
// step (plan row S-07); `.powi(2)` became a product (ADR-8012 A-4 bans powi in lockstep crates).

//! The deterministic golden harness (ADR-8046, plan row F-02).
//!
//! - `a_flipped_bit_names_the_first_diverging_tick`: one flipped state bit at tick 437 is reported
//!   at tick 437, with the field that moved, both live and against a golden.
//! - `engine_scenarios_match_their_committed_goldens`: the engine scenarios hash to their committed
//!   goldens on this target. Each check prints one `regia-det-golden {json}` line (run with
//!   `--nocapture`); `REGIA_BLESS=1` records this target's hash instead.
//! - `rate_60_vs_120_position_heading_and_flat_shot`: the 60 Hz vs 120 Hz gates on the engine
//!   vehicle.
//! - `planted_simd_scenario_differs_only_on_aarch64` (ADR-8081, plan row F-03): a scenario built on
//!   glam's SIMD types is reported as differing, with a first diverging tick, on aarch64 and as
//!   equal on x86-64. The runners (`cargo xtask det-golden`) read its line beside the clean ones.
//! - `vec3a_dot_has_the_scalar_order_on_every_target`: which glam operations leave the scalar
//!   order on this target (`regia-det-probe {json}`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_detsim::float::det_max;
use regia_detsim::golden::rate_gate::{RateGates, RateModel, flat_shot_impact_ms, integrate_pose, measure};
use regia_detsim::golden::scenarios::{
    CollideScenario, SlopeScenario, SplitMix64, SteerScenario, Vehicle, VehicleParams,
};
use regia_detsim::golden::{
    self, DetError, DetRegistry, DetRun, DetScenario, DetStatus, Golden, GoldenMode, Injection,
    REPORT_PREFIX, StateHasher,
};
use regia_detsim::hover::{HoverBadInput, HoverLibm1000, HoverMixed, LowgBallisticMixed};
use regia_detsim::math;
use regia_detsim::tick::TickRate;

mod planted;

/// Prefix of the glam operation probe line the runners collect (ADR-8081).
const PROBE_PREFIX: &str = "regia-det-probe ";

/// A fresh directory under the system temp dir: pid plus a counter, so parallel tests never share.
fn unique_temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-det-golden-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn a_flipped_bit_names_the_first_diverging_tick() {
    let injection = Injection { tick: 437, bit: 3 };

    // Live: the same scenario twice, one with a flipped mantissa bit after tick 437 steps.
    let mut clean = SteerScenario::new();
    let mut flipped = SteerScenario::new();
    let run = clean.default_run();
    let divergence = golden::compare(&mut clean, &mut flipped, &run, Some(injection))
        .expect("the steering scenario supports injection")
        .expect("a flipped bit must diverge");
    assert_eq!(divergence.tick, 437, "{divergence}");
    assert_ne!(divergence.expected_hash, divergence.actual_hash);
    let first = divergence.fields.first().expect("the diff names a field");
    assert_eq!(first.name, "car[0].pos_x", "{divergence}");
    let (e, a) = (
        first.expected.expect("expected bits"),
        first.actual.expect("actual bits"),
    );
    assert_eq!(
        e ^ a,
        1 << 3,
        "exactly the flipped bit differs at the tick it flipped"
    );
    assert_eq!(
        divergence.fields.len(),
        1,
        "only the flipped field differs at tick 437"
    );

    // Against a golden: the clean run blessed, the flipped run checked.
    let dir = unique_temp_dir("flip");
    let path = golden::golden_path(&dir, "steer_slalom");
    let clean_trace = golden::record(&mut SteerScenario::new(), &run);
    let blessed = golden::check_trace(&clean_trace, &path, "x86_64-linux", GoldenMode::Bless);
    assert_eq!(blessed.status, DetStatus::Blessed, "{blessed}");
    let flipped_trace = golden::record_injected(&mut SteerScenario::new(), &run, Some(injection))
        .expect("injection supported");
    for target in ["x86_64-linux", "aarch64-android"] {
        let report = golden::check_trace(&flipped_trace, &path, target, GoldenMode::Check);
        assert_eq!(report.status, DetStatus::Diverged, "{report}");
        assert_eq!(report.first_diverging_tick, Some(437), "{report}");
        assert_eq!(report.expected_tick_hash, Some(clean_trace.tick_hashes[437]));
        assert_eq!(report.actual_tick_hash, Some(flipped_trace.tick_hashes[437]));
        assert!(report.detail.contains("tick 437"), "{report}");
        let json = report.to_json();
        assert!(json.contains("\"first_diverging_tick\":437"), "{json}");
        assert!(json.contains("\"status\":\"diverged\""), "{json}");
    }
    let clean_check = golden::check_trace(&clean_trace, &path, "x86_64-linux", GoldenMode::Check);
    assert_eq!(clean_check.status, DetStatus::Ok, "{clean_check}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_engine_scenario_names_its_flipped_field() {
    for (mut a, mut b, field) in [
        (
            Box::new(SlopeScenario::new()) as Box<dyn DetScenario>,
            Box::new(SlopeScenario::new()) as Box<dyn DetScenario>,
            "sled[0].pos_x",
        ),
        (
            Box::new(CollideScenario::new()),
            Box::new(CollideScenario::new()),
            "disc[0].pos_x",
        ),
        (
            Box::new(HoverLibm1000::new()),
            Box::new(HoverLibm1000::new()),
            "pos_x",
        ),
        (
            Box::new(HoverBadInput::new()),
            Box::new(HoverBadInput::new()),
            "pos_x",
        ),
    ] {
        let mut run = a.default_run();
        run.ticks = 200;
        let injection = Injection { tick: 150, bit: 0 };
        let d = golden::compare(a.as_mut(), b.as_mut(), &run, Some(injection))
            .expect("injection supported")
            .expect("diverges");
        assert_eq!(d.tick, 150, "{}: {d}", a.name());
        assert_eq!(d.fields[0].name, field, "{d}");
    }
}

#[test]
fn identical_runs_never_diverge_and_another_seed_diverges_at_tick_zero() {
    let mut a = CollideScenario::new();
    let mut b = CollideScenario::new();
    let run = a.default_run();
    assert_eq!(
        golden::compare(&mut a, &mut b, &run, None).expect("no injection"),
        None
    );
    let first = golden::record(&mut a, &run);
    let second = golden::record(&mut a, &run);
    assert_eq!(first, second, "a reset scenario replays the same bits");
    assert!(a.contacts() > 0, "the collision scenario must collide");

    let other = DetRun {
        seed: run.seed ^ 1,
        ..run
    };
    let moved = golden::record(&mut b, &other);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &moved.tick_hashes),
        Some(0)
    );
}

#[test]
fn first_divergence_of_prefixes_is_past_the_shorter_end() {
    assert_eq!(golden::first_divergence(&[1, 2, 3], &[1, 2, 3]), None);
    assert_eq!(golden::first_divergence(&[1, 2, 3], &[1, 2]), Some(2));
    assert_eq!(golden::first_divergence(&[1], &[1, 9]), Some(1));
    assert_eq!(golden::first_divergence(&[1, 2, 3], &[1, 7, 3]), Some(1));
    assert_eq!(golden::first_divergence(&[], &[]), None);
}

#[test]
fn engine_scenarios_match_their_committed_goldens() {
    let registry = DetRegistry::with_engine_scenarios();
    let names: Vec<&str> = registry.entries().iter().map(|e| e.name()).collect();
    assert_eq!(
        names,
        [
            "steer_slalom",
            "slope_follow",
            "disc_collide",
            "hover_libm_1000",
            "hover_bad_input",
            "math_rng_fixed_vectors",
            "hover_mixed",
            "lowg_ballistic_mixed",
            "hover_fleet_terrain",
            "projectile_mix",
            "input_log_arena",
            "contact_pile",
            "ground_vehicle_mix"
        ]
    );
    let mut failed = Vec::new();
    for report in registry.check_all(GoldenMode::from_env()) {
        println!("{REPORT_PREFIX}{}", report.to_json());
        if !report.passed() {
            failed.push(report.to_string());
        }
    }
    assert!(
        failed.is_empty(),
        "determinism goldens differ on {}:\n{}",
        golden::current_target(),
        failed.join("\n")
    );
}

#[test]
fn committed_goldens_record_the_reference_trace() {
    for entry in DetRegistry::with_engine_scenarios().entries() {
        let path = golden::golden_path(entry.golden_dir(), entry.name());
        let text = std::fs::read_to_string(&path).expect("committed golden");
        let g = Golden::parse(&text).expect("committed golden parses");
        assert_eq!(g.scenario, entry.name());
        assert_eq!(g.run, entry.create().default_run(), "{}", path.display());
        assert_eq!(
            g.trace_target.as_deref(),
            Some(golden::REFERENCE_TARGET),
            "{}",
            path.display()
        );
        assert_eq!(g.tick_hashes.len(), g.run.ticks as usize);
        assert_eq!(
            g.hash_for(golden::REFERENCE_TARGET),
            Some(golden::run_hash(&g.tick_hashes)),
            "the reference hash is the hash of the committed trace"
        );
        assert_eq!(Golden::parse(&g.to_text()), Ok(g.clone()), "round trip");
    }
}

/// A scenario from outside the engine: a game registers it with its own golden directory.
struct Counter {
    value: u64,
    step: u64,
}

impl DetScenario for Counter {
    // This synthetic scenario exercises intentionally different per-target hashes.
    fn planted(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "outside_counter"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(7, 50, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.value = run.seed;
        self.step = run.seed * 3 + 1;
    }

    fn step(&mut self, tick: u32) {
        self.value = self.value.wrapping_mul(self.step).wrapping_add(u64::from(tick));
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("value", self.value);
    }
}

#[test]
fn an_outside_scenario_registers_blesses_and_checks_per_target() {
    let dir = unique_temp_dir("outside");
    let mut registry = DetRegistry::with_engine_scenarios();
    registry
        .register(dir.clone(), || {
            Box::new(Counter { value: 0, step: 0 }) as Box<dyn DetScenario>
        })
        .expect("a new name registers");
    assert_eq!(
        registry
            .register(dir.clone(), || Box::new(Counter { value: 0, step: 0 })
                as Box<dyn DetScenario>)
            .map_err(|e| e.code()),
        Err("duplicate_scenario")
    );
    let entry = registry.get("outside_counter").expect("registered");
    assert_eq!(entry.golden_dir(), dir.as_path());

    let mut scenario = entry.create();
    let missing = golden::check_golden(scenario.as_mut(), &dir, GoldenMode::Check);
    assert_eq!(missing.status, DetStatus::GoldenMissing, "{missing}");
    assert!(!missing.passed());

    let run = scenario.default_run();
    let trace = golden::record(scenario.as_mut(), &run);
    let path = golden::golden_path(&dir, "outside_counter");
    let blessed = golden::check_trace(&trace, &path, "x86_64-linux", GoldenMode::Bless);
    assert_eq!(blessed.status, DetStatus::Blessed);

    // Another target with no line of its own passes when it equals the reference.
    let other = golden::check_trace(&trace, &path, "aarch64-macos", GoldenMode::Check);
    assert_eq!(other.status, DetStatus::UnrecordedTarget, "{other}");
    assert!(other.passed());
    assert_eq!(other.agrees_with_reference, Some(true));

    // A target may record its own hash; the reference trace stays the x86-64 one.
    let mut odd = trace.clone();
    odd.tick_hashes[49] ^= 1;
    let odd_bless = golden::check_trace(&odd, &path, "aarch64-macos", GoldenMode::Bless);
    assert_eq!(odd_bless.status, DetStatus::Blessed);
    let text = std::fs::read_to_string(&path).expect("golden written");
    let g = Golden::parse(&text).expect("parses");
    assert_eq!(g.trace_target.as_deref(), Some("x86_64-linux"));
    assert_eq!(g.tick_hashes, trace.tick_hashes);
    assert_eq!(g.hash_for("aarch64-macos"), Some(odd.run_hash()));
    let own = golden::check_trace(&odd, &path, "aarch64-macos", GoldenMode::Check);
    assert_eq!(own.status, DetStatus::Ok, "{own}");
    assert_eq!(own.agrees_with_reference, Some(false), "{own}");
    let reference = golden::check_trace(&trace, &path, "aarch64-macos", GoldenMode::Check);
    assert_eq!(reference.status, DetStatus::Diverged, "{reference}");
    assert_eq!(
        reference.first_diverging_tick, None,
        "every tick equals the reference trace"
    );

    // A changed run is stale, not diverged.
    let mut longer = run;
    longer.ticks = 51;
    let stale = golden::check_trace(
        &golden::record(scenario.as_mut(), &longer),
        &path,
        "x86_64-linux",
        GoldenMode::Check,
    );
    assert_eq!(stale.status, DetStatus::GoldenStale, "{stale}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_names_and_bad_goldens_have_codes() {
    for name in ["", "Upper", "has space", "dash-ed", &"x".repeat(65)] {
        assert_eq!(
            golden::validate_name(name).map_err(|e| e.code()),
            Err("bad_scenario_name"),
            "{name:?}"
        );
    }
    assert_eq!(golden::validate_name("steer_slalom_2"), Ok(()));

    let good = Golden::from_trace(
        &golden::record(&mut SteerScenario::new(), &DetRun::new(1, 3, TickRate::DEFAULT)),
        "x86_64-linux",
    )
    .to_text();
    assert!(Golden::parse(&good).is_ok(), "{good}");
    let cases = [
        good.replace("format=regia-det-golden-1", "format=regia-det-golden-0"),
        good.replace("ticks=3", "ticks=4"),
        good.replace("rate_hz=60", "rate_hz=0"),
        good.replace("scenario=steer_slalom", "scenario=Steer"),
        format!("{good}bogus=1\n"),
        format!("{good}hash.x86_64-linux=00\n"),
        good.replace("trace_target=x86_64-linux", "trace_target=aarch64-macos"),
        good.lines()
            .filter(|l| !l.starts_with("seed="))
            .collect::<Vec<_>>()
            .join("\n"),
        good.replacen("t=", "t=zz", 1),
    ];
    for bad in cases {
        match Golden::parse(&bad) {
            Err(DetError::GoldenInvalid(_)) => {}
            other => panic!("expected golden_invalid for\n{bad}\ngot {other:?}"),
        }
    }

    let dir = unique_temp_dir("invalid");
    let path = golden::golden_path(&dir, "steer_slalom");
    std::fs::write(&path, "format=nope\n").expect("write");
    let trace = golden::record(&mut SteerScenario::new(), &DetRun::new(1, 3, TickRate::DEFAULT));
    let report = golden::check_trace(&trace, &path, "x86_64-linux", GoldenMode::Check);
    assert_eq!(report.status, DetStatus::GoldenInvalid, "{report}");
    assert!(report.to_json().contains("\"status\":\"golden_invalid\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn injection_needs_scenario_support() {
    let mut c = Counter { value: 0, step: 0 };
    let run = c.default_run();
    let err = golden::record_injected(&mut c, &run, Some(Injection { tick: 3, bit: 0 }))
        .expect_err("Counter cannot flip a bit");
    assert_eq!(err.code(), "injection_unsupported");
}

#[test]
fn report_json_escapes_and_nulls() {
    let dir = unique_temp_dir("json");
    let trace = golden::record(&mut SteerScenario::new(), &DetRun::new(1, 3, TickRate::DEFAULT));
    let mut report = golden::check_trace(
        &trace,
        &golden::golden_path(&dir, "steer_slalom"),
        "x86_64-linux",
        GoldenMode::Check,
    );
    let json = report.to_json();
    assert!(json.starts_with("{\"scenario\":\"steer_slalom\""), "{json}");
    assert!(json.contains("\"expected\":null"), "{json}");
    assert!(json.contains("\"first_diverging_tick\":null"), "{json}");
    assert!(json.contains("\"status\":\"golden_missing\""), "{json}");
    assert!(json.ends_with('}'));
    // Paths and details are escaped (a `"` cannot be in a Windows file name, so set it directly).
    report.golden = PathBuf::from("dir \"quoted\"\\x");
    report.detail = "line\nbreak\u{1}".into();
    let json = report.to_json();
    assert!(json.contains("\"golden\":\"dir \\\"quoted\\\"\\\\x\""), "{json}");
    assert!(json.contains("\"detail\":\"line\\nbreak\\u0001\""), "{json}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zero_order_hold_takes_the_last_sample_at_or_before_each_tick() {
    // (t_ms, value)
    let samples = [(0u64, 'a'), (20, 'b'), (40, 'c'), (100, 'd')];
    let held = golden::resample_zoh(&samples, 1.0 / 60.0, |s| s.0);
    let values: String = held.iter().map(|s| s.1).collect();
    // Ticks at 0, 16.7, 33.3, 50, 66.7, 83.3 and 100 ms: `d` (100 ms) holds only at the last.
    assert_eq!(values, "aabcccd");
    let empty: [(u64, char); 0] = [];
    assert!(golden::resample_zoh(&empty, 1.0 / 60.0, |s| s.0).is_empty());
}

#[test]
fn splitmix_is_the_published_sequence() {
    // First outputs of SplitMix64 seeded with 1234567 (the reference implementation's test seed).
    let mut r = SplitMix64::new(1_234_567);
    assert_eq!(r.next_u64(), 6_457_827_717_110_365_317);
    assert_eq!(r.next_u64(), 3_203_168_211_198_807_973);
    let mut u = SplitMix64::new(9);
    for _ in 0..1000 {
        let x = u.unit();
        assert!((0.0..1.0).contains(&x));
    }
}

#[test]
fn rate_60_vs_120_position_heading_and_flat_shot() {
    let mut model = Vehicle::new(VehicleParams::default());

    // --- Position, flat ground, constant full throttle ---
    let (x60_1, z60_1, _, _) = integrate_pose(&mut model, 60, 1.0, [1.0, 0.0, 0.0]);
    let (x120_1, z120_1, _, _) = integrate_pose(&mut model, 120, 1.0, [1.0, 0.0, 0.0]);
    let err_1 = math::sqrt((x60_1 - x120_1) * (x60_1 - x120_1) + (z60_1 - z120_1) * (z60_1 - z120_1));
    assert!(
        err_1 <= 0.25,
        "position gate @ 1 s: horizontal error {err_1} m > 0.25 (60=({x60_1},{z60_1}) 120=({x120_1},{z120_1}))"
    );

    let (x60_5, z60_5, _, _) = integrate_pose(&mut model, 60, 5.0, [1.0, 0.0, 0.0]);
    let (x120_5, z120_5, _, _) = integrate_pose(&mut model, 120, 5.0, [1.0, 0.0, 0.0]);
    let err_5 = math::sqrt((x60_5 - x120_5) * (x60_5 - x120_5) + (z60_5 - z120_5) * (z60_5 - z120_5));
    assert!(
        err_5 <= 1.0,
        "position gate @ 5 s: horizontal error {err_5} m > 1.0 (60=({x60_5},{z60_5}) 120=({x120_5},{z120_5}))"
    );

    // --- Heading: constant full steer at rest for 2 s ---
    let (_, _, yaw60, _) = integrate_pose(&mut model, 60, 2.0, [0.0, 0.0, 1.0]);
    let (_, _, yaw120, _) = integrate_pose(&mut model, 120, 2.0, [0.0, 0.0, 1.0]);
    let yaw_err_deg = (yaw60 - yaw120).abs() * 180.0 / std::f32::consts::PI;
    assert!(
        yaw_err_deg <= 2.0,
        "heading gate @ 2 s: |Δyaw|={yaw_err_deg}° > 2° (60={yaw60} 120={yaw120})"
    );

    // --- Flat shot: time-to-impact at 80 u range, 160 u/s muzzle ---
    let speed = 160.0f32;
    let range = 80.0f32;
    let t60 = flat_shot_impact_ms(60, speed, range);
    let t120 = flat_shot_impact_ms(120, speed, range);
    let dt60_ms: f32 = 1000.0 / 60.0;
    let gate = det_max(dt60_ms, 16.0);
    let delta = (t60 - t120).abs();
    assert!(
        delta <= gate,
        "flat-shot gate: |Δt|={delta} ms > max(16, one tick={dt60_ms}) (60={t60} 120={t120})"
    );

    // The model really moved and turned, so the gates measured something.
    assert!(z60_5 > 50.0, "5 s of full throttle covers ground: {z60_5}");
    assert!(yaw60 > 3.0, "2 s of full steer turns: {yaw60}");
}

/// A model that ignores `dt` (moves a fixed step per tick) runs twice as far at 120 Hz: the gates
/// must catch it, or they gate nothing.
struct PerTickModel {
    vel: [f32; 2],
    yaw: f32,
}

impl RateModel for PerTickModel {
    fn reset(&mut self) {
        self.vel = [0.0; 2];
        self.yaw = 0.0;
    }

    fn step(&mut self, input: [f32; 3], _dt: f32) {
        self.vel[1] += input[0] * 0.3;
        self.yaw += input[2] * 0.04;
    }

    fn velocity_xz(&self) -> [f32; 2] {
        self.vel
    }

    fn yaw(&self) -> f32 {
        self.yaw
    }
}

#[test]
fn rate_gates_reject_a_model_that_ignores_dt() {
    let gates = RateGates::default();
    let good = measure(&mut Vehicle::new(VehicleParams::default()), &gates);
    assert!(good.failures(&gates).is_empty(), "{good:?}");
    let bad = measure(
        &mut PerTickModel {
            vel: [0.0; 2],
            yaw: 0.0,
        },
        &gates,
    );
    let failures = bad.failures(&gates);
    assert!(
        failures.iter().any(|f| f.starts_with("position gate @ 1 s")),
        "{failures:?}"
    );
    assert!(
        failures.iter().any(|f| f.starts_with("heading gate")),
        "{failures:?}"
    );
}

#[path = "support/spatial_trace.rs"]
mod spatial_trace;

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "REGIA_SPATIAL_PRINT_TRACE only prints the trace for review; the check is the same (ADR-8367)"
)]
fn spatial_queries_match_the_recorded_sequence_trace() {
    let trace = spatial_trace::trace();
    let path = golden::golden_path(&golden::engine_golden_dir(), "spatial_queries");
    // The initial x86-64 trace is printed for review; this never overwrites a pin.
    // Recording another target is not needed: matching runs report unrecorded_target.
    // Moved 2026-11-20 (ADR-8875, intended): `nearest` stops at the first ring past the incumbent,
    // so the trace's per-call `nearest_candidate` words shrink (243b8a16d8f4dc4b -> f8f97e44669aa31f).
    // Without those words the trace hashes 748bcadbde434bd0 before and after: every range candidate,
    // acceptance, nearest result and update is unchanged.
    if std::env::var_os("REGIA_SPATIAL_PRINT_TRACE").is_some() {
        println!(
            "spatial-trace-begin\n{}spatial-trace-end",
            Golden::from_trace(&trace, &golden::current_target()).to_text()
        );
    }
    let report = golden::check_trace(&trace, &path, &golden::current_target(), GoldenMode::Check);
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(
        matches!(report.status, DetStatus::Ok | DetStatus::UnrecordedTarget),
        "{report}"
    );
    assert_eq!(report.agrees_with_reference, Some(true));
}

#[path = "support/ai_soak_pin.rs"]
mod ai_soak_pin;

/// The AI soak's 600-tick fingerprint is the same on every target (ADR-8561): the det-golden
/// runners carry this test to the aarch64 legs (Android, macOS).
#[test]
fn ai_soak_900_matches_the_pinned_fingerprint_on_every_target() {
    use regia_detsim::golden::DetScenario;
    use regia_detsim::scale_soak::ai::{AiSoakConfig, AiSoakScenario};
    let mut scenario = AiSoakScenario::new(AiSoakConfig::default()).expect("default settings are valid");
    let run = scenario.default_run();
    let hash = golden::record(&mut scenario, &run).run_hash();
    println!(
        "regia-ai-soak-pin {{\"target\":\"{}\",\"hash\":\"{hash:016x}\"}}",
        golden::current_target()
    );
    assert_eq!(hash, ai_soak_pin::AI_SOAK_900_RUN_HASH, "{hash:016x}");
}

#[test]
fn planted_simd_scenario_differs_only_on_aarch64() {
    // Never bless the plant anywhere but the reference target: a blessed aarch64 line would make
    // the divergence the runners must see disappear.
    let target = golden::current_target();
    let mode = match GoldenMode::from_env() {
        GoldenMode::Bless if target == golden::REFERENCE_TARGET => GoldenMode::Bless,
        _ => GoldenMode::Check,
    };
    let report = golden::check_golden(
        &mut planted::PlantedSimd::new(),
        &golden::engine_golden_dir(),
        mode,
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert_eq!(report.scenario, planted::NAME);
    let simd_aarch64 = planted::glam_uses_simd() && cfg!(target_arch = "aarch64");
    if mode == GoldenMode::Bless {
        assert_eq!(report.status, DetStatus::Blessed, "{report}");
    } else if simd_aarch64 {
        assert_eq!(
            report.status,
            DetStatus::Diverged,
            "glam's NEON path must part from the x86-64 SSE2 trace: {report}"
        );
        assert!(report.first_diverging_tick.is_some(), "{report}");
        assert_eq!(report.agrees_with_reference, Some(false), "{report}");
    } else {
        assert!(
            report.passed(),
            "x86-64 (SSE2) and glam's scalar path keep the reference bits: {report}"
        );
        assert_eq!(report.agrees_with_reference, Some(true), "{report}");
    }
}

#[test]
fn vec3a_dot_has_the_scalar_order_on_every_target() {
    let probe = planted::probe(4096);
    println!("{PROBE_PREFIX}{}", probe.to_json(&golden::current_target()));
    assert_eq!(
        probe.vec3a_dot, 0,
        "glam's Vec3A::dot is (x*x' + y*y') + z*z' on every backend: {probe:?}"
    );
    if !planted::glam_uses_simd() {
        assert_eq!(probe.vec3a_cross, 0, "{probe:?}");
        return;
    }
    if cfg!(target_arch = "x86_64") {
        assert_eq!(probe.vec3a_cross, 0, "SSE2 cross rounds every product: {probe:?}");
        assert_eq!(probe.vec4_dot_xz_yw, 0, "SSE2 sums (x+z)+(y+w): {probe:?}");
    }
    if cfg!(target_arch = "aarch64") {
        assert!(probe.vec3a_cross > 0, "NEON cross is fused: {probe:?}");
        assert_eq!(probe.vec4_dot_xy_zw, 0, "NEON sums (x+y)+(z+w): {probe:?}");
    }
}

// Reuse the dependency-free test helper without linking the renderer into cross-target tests.
/// Shared temporary-directory and atomic-write helpers.
#[path = "../../regia_test_utils/src/temp.rs"]
pub mod shared_temp;

fn reference_with_different_tick(trace: &golden::DetTrace, target: &str) -> Golden {
    let mut reference = trace.clone();
    reference.tick_hashes[7] ^= 1;
    Golden::from_trace(&reference, target)
}

#[test]
fn a_blessed_target_that_disagrees_fails_for_a_clean_scenario() {
    let dir = shared_temp::TempDir::new("det-golden-clean-blessed");
    let mut scenario = SteerScenario::new();
    let trace = golden::record(&mut scenario, &scenario_run());
    let path = golden::golden_path(dir.path(), &trace.scenario);
    let target = golden::current_target();
    let reference_target = if target == "x86_64-linux" {
        "aarch64-android"
    } else {
        "x86_64-linux"
    };
    let mut fixture = reference_with_different_tick(&trace, reference_target);
    fixture.set_hash(&target, trace.run_hash());
    shared_temp::write_atomic(&path, fixture.to_text().as_bytes()).expect("write fixture");

    let report = golden::check_golden(&mut scenario, dir.path(), GoldenMode::Check);
    assert_eq!(report.status.code(), "disagrees_with_reference", "{report}");
    assert!(!report.passed());
    assert_eq!(report.agrees_with_reference, Some(false));
    assert_eq!(report.first_diverging_tick, Some(7));
    assert_eq!(report.expected_tick_hash, Some(fixture.tick_hashes[7]));
    assert_eq!(report.actual_tick_hash, Some(trace.tick_hashes[7]));
    assert!(
        report
            .to_json()
            .contains("\"status\":\"disagrees_with_reference\"")
    );
}

fn scenario_run() -> DetRun {
    SteerScenario::new().default_run()
}

#[test]
fn bless_refuses_a_differing_line_for_a_clean_scenario() {
    let dir = shared_temp::TempDir::new("det-golden-clean-refusal");
    let trace = golden::record(&mut SteerScenario::new(), &scenario_run());
    let path = golden::golden_path(dir.path(), &trace.scenario);
    let fixture = reference_with_different_tick(&trace, "x86_64-linux");
    let before = fixture.to_text();
    shared_temp::write_atomic(&path, before.as_bytes()).expect("write fixture");

    let report = golden::check_trace(&trace, &path, "aarch64-android", GoldenMode::Bless);
    assert_eq!(report.status.code(), "bless_refused_clean_differs", "{report}");
    assert!(!report.passed());
    assert_eq!(report.agrees_with_reference, Some(false));
    assert_eq!(report.first_diverging_tick, Some(7));
    assert_eq!(std::fs::read_to_string(&path).expect("read fixture"), before);
    assert!(
        report
            .to_json()
            .contains("\"status\":\"bless_refused_clean_differs\"")
    );
}

#[test]
fn the_planted_scenario_may_differ_per_target() {
    let dir = shared_temp::TempDir::new("det-golden-planted-policy");
    let clean = golden::record(&mut SteerScenario::new(), &scenario_run());
    let mut plant = planted::PlantedSimd::new();
    let run = plant.default_run();
    let planted = golden::record(&mut plant, &run);
    // The explicit plant is the only exception: exercise the same mismatch for both policies.
    for (trace, permitted) in [(&clean, false), (&planted, true)] {
        let path = golden::golden_path(dir.path(), &trace.scenario);
        let fixture = reference_with_different_tick(trace, "x86_64-linux");
        shared_temp::write_atomic(&path, fixture.to_text().as_bytes()).expect("write fixture");
        let report = golden::check_trace(trace, &path, "aarch64-android", GoldenMode::Bless);
        assert_eq!(report.passed(), permitted, "{report}");
        if permitted {
            assert_eq!(report.status, DetStatus::Blessed);
            let checked = golden::check_trace(trace, &path, "aarch64-android", GoldenMode::Check);
            assert!(checked.passed(), "{checked}");
            assert_eq!(checked.agrees_with_reference, Some(false));
            let saved =
                Golden::parse(&std::fs::read_to_string(path).expect("read fixture")).expect("parse fixture");
            assert_eq!(saved.tick_hashes, fixture.tick_hashes);
            assert_eq!(saved.hash_for("aarch64-android"), Some(trace.run_hash()));
        }
    }
}

#[test]
fn golden_text_survives_mutation() {
    for entry in DetRegistry::with_engine_scenarios().entries() {
        let path = golden::golden_path(entry.golden_dir(), entry.name());
        let text = std::fs::read_to_string(path).expect("committed fixture");
        let golden = Golden::parse(&text).expect("valid fixture");
        assert_eq!(Golden::parse(&golden.to_text()), Ok(golden));
        assert!(Golden::parse(&text.replace(golden::FORMAT, "regia-det-golden-2")).is_err());
        // Deterministic ASCII mutations, truncations and unexpected repeated keys.
        for i in (0..text.len()).step_by(17) {
            let mut bytes = text.as_bytes().to_vec();
            bytes[i] = b'!';
            let mutated = std::str::from_utf8(&bytes).expect("ASCII fixture");
            for input in [mutated, &text[..i]] {
                if let Ok(parsed) = Golden::parse(input) {
                    assert_eq!(Golden::parse(&parsed.to_text()), Ok(parsed));
                }
            }
        }
        assert!(Golden::parse(&format!("{text}format={}\n", golden::FORMAT)).is_err());
    }
}

#[test]
fn clean_bless_updates_the_reference_and_records_only_agreeing_targets() {
    let dir = shared_temp::TempDir::new("det-golden-reference-update");
    let trace = golden::record(&mut SteerScenario::new(), &scenario_run());
    assert!(!trace.planted);
    let injected = golden::record_injected(
        &mut SteerScenario::new(),
        &scenario_run(),
        Some(Injection { tick: 7, bit: 0 }),
    )
    .expect("injection");
    assert!(!injected.planted, "injection does not exempt clean scenarios");
    let path = golden::golden_path(dir.path(), &trace.scenario);
    let original = golden::check_trace(&trace, &path, "x86_64-linux", GoldenMode::Bless);
    assert!(original.passed(), "{original}");
    let other = golden::check_trace(&trace, &path, "aarch64-android", GoldenMode::Bless);
    assert!(other.passed(), "{other}");
    assert_eq!(other.agrees_with_reference, Some(true));
    let before = std::fs::read(&path).expect("fixture");
    let refused = golden::check_trace(&injected, &path, "aarch64-android", GoldenMode::Bless);
    assert_eq!(refused.status.code(), "bless_refused_clean_differs");
    assert_eq!(std::fs::read(&path).expect("unchanged fixture"), before);
    // Intentional reference updates remain possible; old secondary hashes cannot hide drift.
    let updated = golden::check_trace(&injected, &path, "x86_64-linux", GoldenMode::Bless);
    assert!(updated.passed(), "{updated}");
    assert_eq!(updated.agrees_with_reference, Some(true));
    let old = golden::check_trace(&trace, &path, "aarch64-android", GoldenMode::Check);
    assert_eq!(old.status.code(), "disagrees_with_reference");
    assert_eq!(old.first_diverging_tick, Some(7));
}

#[test]
fn a_clean_disagreement_without_a_reference_trace_still_fails() {
    let dir = shared_temp::TempDir::new("det-golden-no-reference-trace");
    let trace = golden::record(&mut SteerScenario::new(), &scenario_run());
    let path = golden::golden_path(dir.path(), &trace.scenario);
    let mut fixture = reference_with_different_tick(&trace, "aarch64-android");
    fixture.trace_target = None;
    fixture.tick_hashes.clear();
    // No trace: parsing sorts target lines, so the alphabetically first target is the reference.
    fixture
        .target_hashes
        .push(("x86_64-linux".into(), trace.run_hash()));
    let before = fixture.to_text();
    shared_temp::write_atomic(&path, before.as_bytes()).expect("fixture");
    for (mode, status) in [
        (GoldenMode::Check, "disagrees_with_reference"),
        (GoldenMode::Bless, "bless_refused_clean_differs"),
    ] {
        let report = golden::check_trace(&trace, &path, "x86_64-linux", mode);
        assert_eq!(report.status.code(), status, "{report}");
        assert!(!report.passed());
        assert_eq!(report.agrees_with_reference, Some(false));
        assert_eq!(report.first_diverging_tick, None);
        assert_eq!(report.expected_tick_hash, None);
        assert_eq!(report.actual_tick_hash, None);
        assert_eq!(report.expected, fixture.hash_for("aarch64-android"));
        assert_eq!(std::fs::read_to_string(&path).expect("fixture"), before);
    }
}

#[test]
fn math_rng_fixed_vectors_names_the_family_that_moved() {
    use regia_detsim::golden::vectors::{FAMILIES, MathRngFixedScenario, TICKS};
    let mut a = MathRngFixedScenario::new();
    let mut b = MathRngFixedScenario::new();
    let run = a.default_run();
    assert_eq!(run.ticks, TICKS);
    let injection = Injection { tick: 9, bit: 3 };
    let d = golden::compare(&mut a, &mut b, &run, Some(injection))
        .expect("injection supported")
        .expect("diverges");
    assert_eq!(d.tick, 9);
    assert_eq!(d.fields[0].name, FAMILIES[0]);
    // A reset replays the same bits; another seed moves tick 0 (the table and the streams).
    let first = golden::record(&mut a, &run);
    assert_eq!(first, golden::record(&mut a, &run));
    let other = DetRun {
        seed: run.seed ^ 1,
        ..run
    };
    let moved = golden::record(&mut b, &other);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &moved.tick_hashes),
        Some(0)
    );
    // Every family is fed: no accumulator is still at the FNV offset after a run.
    for family in FAMILIES {
        assert_ne!(a.family_hash(family), Some(0xcbf2_9ce4_8422_2325), "{family}");
    }
}

/// Check `scenario` against its committed golden in the engine golden directory: `ok` on the
/// reference target, `ok` or `unrecorded_target` elsewhere (`blessed` under `REGIA_BLESS=1`). The
/// `regia-det-golden` line for it comes from `engine_scenarios_match_their_committed_goldens`.
fn assert_matches_committed_golden(scenario: &mut dyn DetScenario) {
    let mode = GoldenMode::from_env();
    let report = golden::check_golden(scenario, &golden::engine_golden_dir(), mode);
    assert!(report.passed(), "{report}");
    if mode == GoldenMode::Check && golden::current_target() == golden::REFERENCE_TARGET {
        assert_eq!(report.status, DetStatus::Ok, "{report}");
    }
}

#[test]
fn hover_mixed_matches_its_golden() {
    assert_matches_committed_golden(&mut HoverMixed::new());
}

#[test]
fn lowg_ballistic_mixed_matches_its_golden() {
    assert_matches_committed_golden(&mut LowgBallisticMixed::new());
}

#[test]
fn hover_mixed_reaches_every_branch() {
    for params in HoverMixed::HULLS {
        assert_eq!(params.validate(), Ok(()), "every mixed hull is a valid hull");
    }
    for world in HoverMixed::WORLDS {
        assert!(world.validate().is_ok(), "every mixed world is valid: {world:?}");
    }
    let mut scenario = HoverMixed::new();
    let run = scenario.default_run();
    assert_eq!(run.ticks, 3600, "3600 ticks per world");
    let trace = golden::record(&mut scenario, &run);
    assert_eq!(trace.tick_hashes.len(), 3600);
    let coverage = scenario.coverage();
    for (name, count) in coverage.named() {
        println!("hover_mixed coverage {name}={count}");
        assert!(
            count > 0,
            "hover_mixed must reach {name} (0 of it in the run): {coverage:?}"
        );
    }
    for i in 0..HoverMixed::HULL_COUNT {
        let state = scenario.state(i).expect("hull");
        assert!(state.altitude.is_finite() && state.velocity.iter().all(|v| v.is_finite()));
    }
    // A reset replays the same run and the same counts.
    let again = golden::record(&mut scenario, &run);
    assert_eq!(trace, again);
    assert_eq!(scenario.coverage(), coverage);
}

#[test]
fn lowg_ballistic_mixed_reaches_every_branch() {
    let mut scenario = LowgBallisticMixed::new();
    let run = scenario.default_run();
    let trace = golden::record(&mut scenario, &run);
    assert_eq!(trace.tick_hashes.len(), run.ticks as usize);
    let c = scenario.coverage();
    println!("lowg_ballistic_mixed coverage {c:?}");
    assert!(
        c.tosses > 0 && c.capped_tosses > 0 && c.shells > 0 && c.jumps > 0,
        "{c:?}"
    );
}

#[test]
fn hover_mixed_input_bit_flip_at_1800_diverges_at_1800() {
    let mut clean = HoverMixed::new();
    let mut flipped = HoverMixed::new().with_input_flip(1800, 31);
    let run = clean.default_run();
    let d = golden::compare(&mut clean, &mut flipped, &run, None)
        .expect("no injection")
        .expect("a flipped input bit diverges");
    assert_eq!(d.tick, 1800, "{d}");
    assert!(
        d.fields.iter().any(|f| f.name == "hull[0].in_throttle"),
        "the flipped input is named: {d}"
    );
    assert!(
        d.fields
            .iter()
            .all(|f| f.name.starts_with("hull[0].") || f.name.starts_with("coverage[0].")),
        "only hull 0 (and the counts) move at the flipped tick: {d}"
    );

    // Against a golden: the clean run blessed into a temp dir, the flipped run checked there.
    let dir = unique_temp_dir("mixed-flip");
    let path = golden::golden_path(&dir, "hover_mixed");
    let clean_trace = golden::record(&mut HoverMixed::new(), &run);
    let blessed = golden::check_trace(&clean_trace, &path, golden::REFERENCE_TARGET, GoldenMode::Bless);
    assert_eq!(blessed.status, DetStatus::Blessed, "{blessed}");
    let flipped_trace = golden::record(&mut HoverMixed::new().with_input_flip(1800, 31), &run);
    for target in ["x86_64-linux", "aarch64-android", "aarch64-macos"] {
        let report = golden::check_trace(&flipped_trace, &path, target, GoldenMode::Check);
        assert_eq!(report.status, DetStatus::Diverged, "{report}");
        assert_eq!(report.first_diverging_tick, Some(1800), "{report}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_scenarios_name_their_flipped_field() {
    for (mut a, mut b, field) in [
        (
            Box::new(HoverMixed::new()) as Box<dyn DetScenario>,
            Box::new(HoverMixed::new()) as Box<dyn DetScenario>,
            "hull[0].pos_x",
        ),
        (
            Box::new(LowgBallisticMixed::new()),
            Box::new(LowgBallisticMixed::new()),
            "case[0].debris_x",
        ),
    ] {
        let mut run = a.default_run();
        run.ticks = 1900;
        let injection = Injection { tick: 1800, bit: 0 };
        let d = golden::compare(a.as_mut(), b.as_mut(), &run, Some(injection))
            .expect("injection supported")
            .expect("diverges");
        assert_eq!(d.tick, 1800, "{}: {d}", a.name());
        assert_eq!(d.fields[0].name, field, "{d}");
    }
}

#[test]
fn match_rules_ctf_koth_3600_matches_the_committed_hash() {
    let mut registry = DetRegistry::new();
    registry
        .register(golden::engine_golden_dir(), || {
            Box::new(regia_rules::proof::MatchRulesCtfKoth3600::new()) as Box<dyn DetScenario>
        })
        .expect("match_rules_ctf_koth_3600 registers");
    assert!(registry.get("match_rules_ctf_koth_3600").is_some(), "no scenario");
    let mut failed = Vec::new();
    for report in registry.check_all(GoldenMode::from_env()) {
        println!("{REPORT_PREFIX}{}", report.to_json());
        if !report.passed() {
            failed.push(report.to_string());
        }
    }
    assert!(
        failed.is_empty(),
        "determinism golden differs on {}:\n{}",
        golden::current_target(),
        failed.join("\n")
    );
}
