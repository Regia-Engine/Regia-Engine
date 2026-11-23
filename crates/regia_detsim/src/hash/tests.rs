// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/desync_hash/tests.rs lines 7-185, commit d6ed37677,
// tag regia-absorb-1), ADR-8012. Mark: as-is for the test bodies (ADR-8047). Lines 1-6 (the imports and
// `boot`) are replaced by the prelude below, which binds the generic API to the fixture sim
// (`test_sim.rs`) under the names DO27's tests use; every test body from line 7 on is DO27's text, except
// rustfmt's line breaks, one let-chain in `run_pair` (clippy 1.99 `collapsible_if`; same logic) and one
// det-scan waiver comment in `desync_hash_cost` (ADR-8367; the kit's own scan takes no pins).
// Regia's own tests follow DO27's, after the marker comment.
// Portions copied from Direct Order 27 (ir-sim/src/desync_hash/tests.rs lines 187-201, commit 7a751bac9),
// ADR-8012. Mark: as-is (ADR-8078): `float_identity_ignores_zero_sign_and_nan_payload_only`, before the
// marker comment; see its own comment.

use super::test_sim::{SYSTEMS, TestSim as GameplayRuntimeState, seed_to_bootstrap};
use super::*;

const SYSTEM_COUNT: usize = super::test_sim::SYSTEM_COUNT;
const SYSTEM_NAMES: [&str; SYSTEM_COUNT] = *SYSTEMS.names();
type SystemHashes = super::SystemHashes<SYSTEM_COUNT>;
type SystemRing = super::SystemRing<SYSTEM_COUNT>;
type TraceRow = super::TraceRow<SYSTEM_COUNT>;

fn system_hashes(state: &GameplayRuntimeState) -> SystemHashes {
    super::system_hashes(state)
}

fn inject_divergence(state: &mut GameplayRuntimeState, system: &str) -> bool {
    super::inject_divergence(state, system)
}

fn diverged_systems(left: &[TraceRow], right: &[TraceRow]) -> DivergenceReport {
    super::diverged_systems(&SYSTEMS, left, right)
}

fn desync_report(
    tick: u64,
    left_label: &str,
    left: &SystemHashes,
    right_label: &str,
    right: &SystemHashes,
) -> String {
    super::desync_report(&SYSTEMS, tick, left_label, left, right_label, right)
}

fn parse_row_line(line: &str) -> Option<(String, TraceRow)> {
    super::parse_row_line(line)
}

fn parse_inject(text: &str) -> Option<InjectPlan> {
    super::parse_inject(&SYSTEMS, text)
}

fn boot(seed: u64) -> GameplayRuntimeState {
    GameplayRuntimeState::from_startup_bootstrap(seed_to_bootstrap(seed))
}

/// Step two copies of one boot state for `ticks` ticks and record a ring per peer. On peer B,
/// `system` is nudged once, just before the step that ends on label `at` (labels are the
/// post-step `state.tick`).
fn run_pair(seed: u64, ticks: u64, inject: Option<(&str, u64)>) -> (SystemRing, SystemRing) {
    let mut a = boot(seed);
    let mut b = a.clone();
    let mut ring_a = SystemRing::new();
    let mut ring_b = SystemRing::new();
    for _ in 0..ticks {
        if let Some((system, at)) = inject
            && b.tick + 1 == at
        {
            assert!(inject_divergence(&mut b, system), "cannot inject into {system}");
        }
        a.advance_tick();
        b.advance_tick();
        assert_eq!(a.tick, b.tick);
        ring_a.record(a.tick, &a);
        ring_b.record(b.tick, &b);
    }
    (ring_a, ring_b)
}

#[test]
fn identical_runs_never_diverge() {
    let (a, b) = run_pair(3, 40, None);
    let report = diverged_systems(&a.window(40, 40), &b.window(40, 40));
    assert_eq!(report.compared, 40);
    assert!(report.systems.is_empty(), "{}", report.list());
    assert_eq!(report.last_agreed, Some(40));
}

#[test]
fn vector_is_deterministic_across_clones() {
    let state = boot(5);
    assert_eq!(system_hashes(&state), system_hashes(&state.clone()));
}

#[test]
fn every_system_injection_is_named_at_its_tick() {
    const AT: u64 = 12;
    for system in SYSTEM_NAMES {
        let (a, b) = run_pair(3, AT + 6, Some((system, AT)));
        let report = diverged_systems(&a.window(AT + 6, 64), &b.window(AT + 6, 64));
        let first_for = |name: &str| {
            report
                .systems
                .iter()
                .find(|d| d.system == name)
                .map(|d| d.first_tick)
        };
        assert_eq!(first_for(system), Some(AT), "{system}: {}", report.list());
        let root = report.root().unwrap_or_else(|| panic!("{system}: nothing named"));
        assert_eq!(
            root.first_tick,
            AT,
            "{system}: split before the injection: {}",
            report.list()
        );
        assert_eq!(report.last_agreed, Some(AT - 1), "{system}");
        let lines = divergence_lines(AT + 6, "a", "b", &report);
        assert!(lines[0].contains(&format!("at tick {AT}")), "{lines:?}");
    }
}

/// Fields no other system reads back name only themselves.
#[test]
fn isolated_injections_name_exactly_one_system() {
    const AT: u64 = 9;
    for system in [
        "economy",
        "production",
        "ai",
        "pilots",
        "buildings",
        "terrain",
        "match",
        "rng",
        "training",
        "projectiles",
        "weapons",
    ] {
        let (a, b) = run_pair(3, AT + 4, Some((system, AT)));
        let report = diverged_systems(&a.window(AT + 4, 64), &b.window(AT + 4, 64));
        let at_split: Vec<&str> = report
            .systems
            .iter()
            .filter(|d| d.first_tick == AT)
            .map(|d| d.system)
            .collect();
        assert_eq!(at_split, vec![system], "{}", report.list());
    }
}

#[test]
fn a_late_split_keeps_the_earlier_ticks_clean() {
    let (a, b) = run_pair(7, 30, Some(("economy", 25)));
    let report = diverged_systems(&a.window(30, 64), &b.window(30, 64));
    assert_eq!(report.last_agreed, Some(24));
    assert_eq!(
        report.root().map(|d| (d.system, d.first_tick)),
        Some(("economy", 25))
    );
}

#[test]
fn ring_keeps_its_capacity_and_serves_windows() {
    let mut ring = SystemRing::new();
    for tick in 1..=300u64 {
        ring.push(
            tick,
            SystemHashes {
                words: [tick; SYSTEM_COUNT],
            },
        );
    }
    assert_eq!(ring.len(), RING_TICKS);
    let rows = ring.window(300, 30);
    assert_eq!(rows.len(), 30);
    assert_eq!(rows[0].tick, 271);
    assert_eq!(rows[29].tick, 300);
    assert!(ring.window(30, 30).is_empty(), "scrolled out");
    assert_eq!(ring.window(300, 5000).len(), MAX_TRACE_ROWS, "bounded");
}

#[test]
fn ring_rewind_replaces_the_tail() {
    let mut ring = SystemRing::new();
    for tick in 1..=10u64 {
        ring.push(tick, SystemHashes::default());
    }
    ring.push(
        6,
        SystemHashes {
            words: [9; SYSTEM_COUNT],
        },
    );
    assert_eq!(ring.len(), 6);
    assert_eq!(ring.window(6, 1)[0].hashes.words[0], 9);
}

#[test]
fn row_lines_round_trip() {
    let mut hashes = SystemHashes::default();
    for (i, word) in hashes.words.iter_mut().enumerate() {
        *word = 0x0123_4567_89ab_cdef ^ (i as u64);
    }
    let row = TraceRow { tick: 4242, hashes };
    let line = row_line("host", &row);
    let (peer, back) = parse_row_line(&format!("2026-10-23T10:00:00Z WARN {line}")).expect("parses");
    assert_eq!(peer, "host");
    assert_eq!(back, row);
    assert!(parse_row_line("lockstep systems row host 5 00:11").is_none());
}

#[test]
fn missing_rows_are_skipped_not_compared() {
    let mut a = SystemRing::new();
    let mut b = SystemRing::new();
    for tick in 1..=6u64 {
        a.push(tick, SystemHashes::default());
        if tick >= 4 {
            let mut h = SystemHashes::default();
            if tick == 5 {
                h.words[4] = 1;
            }
            b.push(tick, h);
        }
    }
    let report = diverged_systems(&a.window(6, 64), &b.window(6, 64));
    assert_eq!(report.compared, 3);
    assert_eq!(
        report.root().map(|d| (d.system, d.first_tick)),
        Some(("economy", 5))
    );
}

#[test]
fn inject_plans_parse() {
    let plan = parse_inject(" economy@120#2 ").expect("with seat");
    assert_eq!((plan.system, plan.step, plan.seat), ("economy", 120, Some(2)));
    let plan = parse_inject("units@7").expect("no seat");
    assert_eq!((plan.system, plan.step, plan.seat), ("units", 7, None));
    assert!(parse_inject("nonsense@7").is_none());
    assert!(parse_inject("economy").is_none());
    assert!(parse_inject("economy@x").is_none());
}

#[test]
fn equal_states_name_no_system() {
    let state = boot(3);
    let report = desync_report(1, "a", &system_hashes(&state), "b", &system_hashes(&state));
    assert!(report.contains("first none"), "{report}");
}

/// `cargo test -p ir-sim --release desync_hash_cost -- --ignored --nocapture`
#[test]
#[ignore]
fn desync_hash_cost() {
    use std::time::Instant;
    for (label, scale) in [("boot", 0usize), ("x4", 4), ("x8", 8), ("x20", 20), ("x100", 100)] {
        let mut state = boot(3);
        let base: Vec<_> = state.runtime_entities.clone();
        let mut next = 50_000u32;
        for _ in 0..scale {
            for entity in &base {
                let mut copy = entity.clone();
                copy.entity_id = next;
                next += 1;
                state.runtime_entities.push(copy);
            }
        }
        let n = 2000u32;
        // det-scan: allow(wall_clock): the #[ignore]d perf probe prints a cost; it never steps a sim
        let start = Instant::now();
        let mut sink = 0u64;
        for _ in 0..n {
            sink ^= system_hashes(&state).words[0];
        }
        let per = start.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        println!(
            "desync_hash_cost {label}: {} entities, {per:.4} ms per tick ({sink:x})",
            state.runtime_entities.len()
        );
    }
}

// Portions copied from Direct Order 27 (ir-sim/src/desync_hash/tests.rs lines 187-201, commit
// 7a751bac91aa36cb0a3e3df718fbce1c9ba52ff8), ADR-8012. Mark: as-is (ADR-8078); the test name drops DO27's lane
// prefix, the body is DO27's text except rustfmt's line breaks.
#[test]
fn float_identity_ignores_zero_sign_and_nan_payload_only() {
    assert_eq!(canonical_f32_bits(-0.0), canonical_f32_bits(0.0));
    assert_eq!(canonical_f32_bits(0.0), 0);
    assert_eq!(canonical_f32_bits(f32::from_bits(0x7fc0_0001)), 0x7fc0_0000);
    assert_eq!(canonical_f32_bits(f32::from_bits(0xffa0_1234)), 0x7fc0_0000);
    // Real differences stay visible: finite values, subnormals and infinities keep their bits.
    for v in [
        1.0f32,
        -1.0,
        f32::MIN_POSITIVE / 2.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1.0e-30,
    ] {
        assert_eq!(canonical_f32_bits(v), v.to_bits());
    }
    assert_ne!(
        canonical_f32_bits(f32::INFINITY),
        canonical_f32_bits(f32::NEG_INFINITY)
    );
    assert_eq!(canonical_f64_bits(-0.0), 0);
    assert_eq!(
        canonical_f64_bits(f64::from_bits(0x7ff0_0000_0000_0001)),
        0x7ff8_0000_0000_0000
    );
    assert_eq!(canonical_f64_bits(2.5), 2.5f64.to_bits());
}

// ---- Regia tests (not from DO27) ----

#[test]
fn negative_zero_and_nan_hash_canonically() {
    let fold = |value: &dyn Fn(&mut Mix)| {
        let mut mix = Mix::new(7);
        value(&mut mix);
        mix.finish()
    };
    // f32, direct and through the serializer.
    let zero = fold(&|m| m.f32(0.0));
    assert_eq!(fold(&|m| m.f32(-0.0)), zero, "-0.0f32 must hash as +0.0");
    assert_eq!(fold(&|m| m.fold(&-0.0f32)), zero, "serde -0.0f32");
    let nan = fold(&|m| m.f32(f32::NAN));
    for bits in [
        0x7fc0_0000u32,
        0xffc0_0000,
        0x7fc0_0001,
        0x7f80_0001,
        0xffff_ffff,
        0x7fbf_ffff,
    ] {
        let v = f32::from_bits(bits);
        assert!(v.is_nan());
        assert_eq!(fold(&|m| m.f32(v)), nan, "f32 NaN {bits:#010x}");
        assert_eq!(fold(&|m| m.fold(&v)), nan, "serde f32 NaN {bits:#010x}");
    }
    assert_ne!(nan, zero);
    assert_ne!(
        fold(&|m| m.f32(1.0)),
        fold(&|m| m.f32(-1.0)),
        "only zero loses its sign"
    );
    assert_ne!(fold(&|m| m.f32(f32::INFINITY)), nan);
    // f64.
    let zero64 = fold(&|m| m.f64(0.0));
    assert_eq!(fold(&|m| m.f64(-0.0)), zero64, "-0.0f64 must hash as +0.0");
    assert_eq!(fold(&|m| m.fold(&-0.0f64)), zero64, "serde -0.0f64");
    let nan64 = fold(&|m| m.f64(f64::NAN));
    for bits in [
        0xfff8_0000_0000_0000u64,
        0x7ff8_0000_0000_0001,
        0x7ff0_0000_0000_0001,
        u64::MAX,
    ] {
        let v = f64::from_bits(bits);
        assert!(v.is_nan());
        assert_eq!(fold(&|m| m.f64(v)), nan64, "f64 NaN {bits:#018x}");
        assert_eq!(fold(&|m| m.fold(&v)), nan64, "serde f64 NaN {bits:#018x}");
    }
    // Inside a game's state: a body whose velocity went to -0.0 on one peer only names no system.
    let a = boot(3);
    let mut b = a.clone();
    let (mut ra, mut rb) = (SystemRing::new(), SystemRing::new());
    b.runtime_entities[2].velocity = [-0.0, -0.0, -0.0];
    b.runtime_entities[2].position[1] = f32::from_bits(0xffc0_0001);
    let mut a = a;
    a.runtime_entities[2].position[1] = f32::NAN;
    ra.record(1, &a);
    rb.record(1, &b);
    let report = diverged_systems(&ra.window(1, 1), &rb.window(1, 1));
    assert!(report.systems.is_empty(), "{}", report.list());
}

#[test]
fn canonical_bits_keep_every_other_value() {
    for v in [
        0.0f32,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::MAX,
        f32::NEG_INFINITY,
    ] {
        assert_eq!(canonical_f32_bits(v), v.to_bits(), "{v:e}");
    }
    assert_eq!(canonical_f32_bits(-0.0), 0);
    assert_eq!(canonical_f32_bits(f32::from_bits(0xff80_0001)), CANONICAL_NAN_F32);
    for v in [0.0f64, 1.0, -1.0, f64::from_bits(1), f64::MAX, f64::NEG_INFINITY] {
        assert_eq!(canonical_f64_bits(v), v.to_bits(), "{v:e}");
    }
    assert_eq!(canonical_f64_bits(-0.0), 0);
    assert_eq!(
        canonical_f64_bits(f64::from_bits(0xfff0_0000_0000_0001)),
        CANONICAL_NAN_F64
    );
}

/// The word mix is DO27's, bit for bit: these words are pinned so a change to the mix, the finish or
/// the serializer's encoding of a serde shape fails here (a change is a sim-version change, ADR-8047).
#[test]
fn mix_words_are_pinned() {
    let mut words = Mix::new(1);
    for w in [0u64, 1, u64::MAX, 0x0123_4567_89ab_cdef] {
        words.u64(w);
    }
    let mut shapes = Mix::new(2);
    shapes.fold(&(
        true,
        -3i8,
        -70_000i32,
        u128::MAX - 5,
        1.5f32,
        -2.25f64,
        'x',
        "a string of 21 bytes.",
        Some(4u16),
        None::<u8>,
        vec![1u32, 2, 3],
    ));
    let mut map = std::collections::BTreeMap::new();
    map.insert(2u32, [0.5f32, -0.0]);
    map.insert(1u32, [f32::NAN, 3.0]);
    let mut maps = Mix::new(3);
    maps.fold(&map);
    let state = system_hashes(&boot(3));
    let got = [
        words.finish(),
        shapes.finish(),
        maps.finish(),
        state.words[0],
        state.words[12],
        state.combined(),
    ];
    assert_eq!(got, PINNED_MIX_WORDS, "got {got:#018x?}");
}

/// Word 0 was also recomputed by an independent script of the mix (lane F-04 report); words 0, 1, 3, 4
/// and 5 are the same with DO27's raw float bits (no -0.0 or NaN in them), word 2 is not (its map holds
/// both).
const PINNED_MIX_WORDS: [u64; 6] = [
    0x1b18_78be_df70_f221,
    0xcfc0_52c0_2278_c514,
    0xa1ba_272e_cd20_c033,
    0x118b_df72_9ff8_1296,
    0x045a_6add_4efe_45c4,
    0x108f_6bec_67b9_5342,
];

#[test]
fn system_hashes_serialize_as_a_words_tuple() {
    // The shape a derive gives `struct SystemHashes { words: [u64; 13] }`: one struct field holding a
    // 13-tuple, so `Mix` folds it exactly as DO27's derived type.
    let mut h = SystemHashes::default();
    for (i, word) in h.words.iter_mut().enumerate() {
        *word = (i as u64 + 1) * 0x1111;
    }
    #[derive(serde::Serialize)]
    struct Derived {
        words: [u64; SYSTEM_COUNT],
    }
    let mut a = Mix::new(9);
    a.fold(&h);
    let mut b = Mix::new(9);
    b.fold(&Derived { words: h.words });
    assert_eq!(a.finish(), b.finish());
    let row = TraceRow { tick: 77, hashes: h };
    let text = serde_json::to_string(&row).expect("serializes");
    let back: TraceRow = serde_json::from_str(&text).expect("round trips");
    assert_eq!(back, row);
    assert!(
        serde_json::from_str::<TraceRow>(r#"{"tick":1,"hashes":{"words":[1,2]}}"#).is_err(),
        "short vector"
    );
}

#[test]
fn desync_hash_rows_follow_the_system_order() {
    struct Wrap(GameplayRuntimeState);
    impl DesyncHash for Wrap {
        fn world_hash(&self, _tick: u64) -> u64 {
            super::system_hashes(&self.0).combined()
        }
        fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
            write_rows(&SYSTEMS, &super::system_hashes(&self.0), out);
        }
    }
    let sim = Wrap(boot(3));
    let mut rows = vec![("stale", 1)];
    sim.subsystem_hashes(0, &mut rows);
    let names: Vec<&str> = rows.iter().map(|r| r.0).collect();
    assert_eq!(names, SYSTEM_NAMES);
    let h = system_hashes(&sim.0);
    assert_eq!(rows[4].1, h.words[4]);
    assert_eq!(sim.world_hash(0), h.combined());
    let mut other = sim.0.clone();
    assert!(inject_divergence(&mut other, "rng"));
    assert_ne!(
        Wrap(other).world_hash(0),
        sim.world_hash(0),
        "world hash covers every system"
    );
}

#[test]
fn unknown_names_and_unsupported_systems_do_not_inject() {
    let mut state = boot(3);
    let before = system_hashes(&state);
    assert!(!inject_divergence(&mut state, "nonsense"));
    assert_eq!(system_hashes(&state), before);
    struct NoInject;
    impl SystemFold<1> for NoInject {
        const SYSTEMS: Systems<1> = Systems::new(["only"]);
        fn fold_systems(&self, mix: &mut [Mix; 1]) {
            mix[0].u64(1);
        }
    }
    assert!(
        !super::inject_divergence(&mut NoInject, "only"),
        "the default inject refuses"
    );
}

#[test]
fn systems_lookups_and_fields() {
    let mut h = SystemHashes::default();
    SYSTEMS.set(&mut h, "match", 0xabc);
    SYSTEMS.set(&mut h, "nonsense", 0xdef);
    assert_eq!(SYSTEMS.get(&h, "match"), Some(0xabc));
    assert_eq!(SYSTEMS.get(&h, "nonsense"), None);
    assert_eq!(SYSTEMS.first_diff(&h, &SystemHashes::default()), Some("match"));
    assert_eq!(SYSTEMS.name(12), Some("training"));
    assert_eq!(SYSTEMS.name(13), None);
    let fields = SYSTEMS.fields(&h);
    assert!(fields.starts_with("units 0000000000000000 physics"), "{fields}");
    assert!(fields.contains("match 0000000000000abc"), "{fields}");
    let report = desync_report(5, "a", &h, "b", &SystemHashes::default());
    assert!(report.starts_with("desync tick 5 first match\n"), "{report}");
}

#[test]
fn sampling_and_the_log_arm() {
    assert!(!should_sample(30, None));
    assert!(!should_sample(30, Some(0)));
    assert!(should_sample(30, Some(10)));
    assert!(!should_sample(31, Some(10)));
    let arm = SystemLogArm::new();
    assert_eq!(arm.take(), None);
    arm.arm(50);
    arm.arm(70);
    arm.arm(40);
    assert_eq!(arm.take(), Some(40), "the earliest tick wins");
    assert_eq!(arm.take(), None, "taken");
    arm.arm(SYSTEM_LOG_TICK_UNKNOWN);
    assert_eq!(arm.take(), Some(SYSTEM_LOG_TICK_UNKNOWN));
}

#[test]
#[should_panic(expected = "duplicate system name")]
fn a_duplicate_system_name_is_refused() {
    let _ = Systems::new(["units", "economy", "units"]);
}

#[test]
#[should_panic(expected = "a system name is non-empty ASCII")]
fn a_system_name_with_a_space_is_refused() {
    let _ = Systems::new(["units", "the economy"]);
}

#[test]
#[should_panic(expected = "at most MAX_SYSTEMS")]
fn more_than_64_systems_are_refused() {
    const NAMES: [&str; 65] = {
        let mut names = ["s"; 65];
        names[0] = "a";
        names
    };
    // Only the length is checked first, so the repeated names never matter.
    let _ = Systems::new(NAMES);
}

/// The canonical rule against an oracle built from the IEEE classes (`classify`, not the bits): a zero of
/// either sign folds to `+0.0`, a NaN of any sign or payload folds to the one quiet NaN, every other value
/// (normal, subnormal, both infinities) keeps its bits. `canonical_f32_bits` and `canonical_f64_bits` are
/// DO27's text (ADR-8078), so this pins that DO27's rule and the IEEE classes agree on `bits`.
fn check_f32_against_the_classes(bits: u32) {
    use std::num::FpCategory;
    let v = f32::from_bits(bits);
    let want = match v.classify() {
        FpCategory::Nan => CANONICAL_NAN_F32,
        FpCategory::Zero => 0,
        FpCategory::Normal | FpCategory::Subnormal | FpCategory::Infinite => bits,
    };
    assert_eq!(canonical_f32_bits(v), want, "f32 bits {bits:#010x}");
}

fn check_f64_against_the_classes(bits: u64) {
    use std::num::FpCategory;
    let v = f64::from_bits(bits);
    let want = match v.classify() {
        FpCategory::Nan => CANONICAL_NAN_F64,
        FpCategory::Zero => 0,
        FpCategory::Normal | FpCategory::Subnormal | FpCategory::Infinite => bits,
    };
    assert_eq!(canonical_f64_bits(v), want, "f64 bits {bits:#018x}");
}

/// The bit patterns around every class edge, for both signs: the zeros, the smallest and largest
/// subnormals, the smallest normal, the largest finite value, the infinity, the first and last
/// signalling NaN, the first quiet NaN and the all-ones pattern.
const F32_EDGES: [u32; 10] = [
    0x0000_0000,
    0x0000_0001,
    0x007f_ffff,
    0x0080_0000,
    0x7f7f_ffff,
    0x7f80_0000,
    0x7f80_0001,
    0x7fbf_ffff,
    0x7fc0_0000,
    0x7fff_ffff,
];

const F64_EDGES: [u64; 10] = [
    0x0000_0000_0000_0000,
    0x0000_0000_0000_0001,
    0x000f_ffff_ffff_ffff,
    0x0010_0000_0000_0000,
    0x7fef_ffff_ffff_ffff,
    0x7ff0_0000_0000_0000,
    0x7ff0_0000_0000_0001,
    0x7ff7_ffff_ffff_ffff,
    0x7ff8_0000_0000_0000,
    0x7fff_ffff_ffff_ffff,
];

#[test]
fn canonical_rule_follows_the_ieee_classes() {
    for edge in F32_EDGES {
        for bits in [edge, edge | 0x8000_0000] {
            for near in [bits.wrapping_sub(1), bits, bits.wrapping_add(1)] {
                check_f32_against_the_classes(near);
            }
        }
    }
    // Every 257th f32 pattern (257 is odd, so the stride visits every low-bit residue and both signs).
    let mut bits = 0u32;
    loop {
        check_f32_against_the_classes(bits);
        match bits.checked_add(257) {
            Some(next) => bits = next,
            None => break,
        }
    }
    for edge in F64_EDGES {
        for bits in [edge, edge | 0x8000_0000_0000_0000] {
            for near in [bits.wrapping_sub(1), bits, bits.wrapping_add(1)] {
                check_f64_against_the_classes(near);
            }
        }
    }
    // f64: every exponent and sign, with a spread of mantissas (low, high, alternating bits).
    for sign_exp in 0u64..0x1000 {
        for mantissa in [
            0u64,
            1,
            2,
            0x0008_0000_0000_0000,
            0x0007_ffff_ffff_ffff,
            0x000f_ffff_ffff_fffe,
            0x000f_ffff_ffff_ffff,
            0x0005_5555_5555_5555,
            0x000a_aaaa_aaaa_aaaa,
        ] {
            check_f64_against_the_classes((sign_exp << 52) | mantissa);
        }
    }
}

/// All 2^32 `f32` patterns against the classes (about 15 s at the dev profile on a busy host). Run with
/// `cargo test -p regia_detsim --lib -- --ignored every_f32`.
#[test]
#[ignore = "exhaustive: 2^32 patterns; run by hand when the rule changes"]
fn every_f32_follows_the_ieee_classes() {
    for bits in 0..=u32::MAX {
        check_f32_against_the_classes(bits);
    }
}

// ---------------------------------------------------------------------------------------------------
// ADR-8547 (Regia's own tests): the raw diagnose ring, partial folds and the mixer's quality.
// ---------------------------------------------------------------------------------------------------

/// A three-system sim whose `motion` system holds a value that may become a signed zero and, later,
/// the reciprocal of it (the way a zero sign turns into a real split: `1/-0.0 = -inf`).
#[derive(Clone, Debug, Default)]
struct ZeroSim {
    tick: u64,
    zero: f32,
    inv: f32,
    pos: [f32; 2],
    gold: u32,
    nan: f64,
}

const ZERO_SYSTEMS: Systems<3> = Systems::new(["units", "motion", "economy"]);

impl SystemFold<3> for ZeroSim {
    const SYSTEMS: Systems<3> = ZERO_SYSTEMS;
    fn fold_systems(&self, mix: &mut [Mix; 3]) {
        mix[0].fold(&self.pos);
        mix[1].f32(self.zero);
        mix[1].f32(self.inv);
        mix[2].u64(u64::from(self.gold));
        mix[2].f64(self.nan);
        mix[2].u64(self.tick);
    }
}

impl ZeroSim {
    fn step(&mut self, tick: u64, recip_at: Option<u64>) {
        self.tick = tick;
        self.pos[0] += 0.25;
        self.gold = self.gold.wrapping_add(3);
        if recip_at == Some(tick) {
            self.inv = 1.0 / self.zero;
        }
    }
}

/// Run two peers to `ticks` on raw rings; peer B's zero turns negative before step `split_at`; both
/// take the reciprocal at `recip_at`.
fn zero_pair(
    ticks: u64,
    split_at: u64,
    recip_at: Option<u64>,
) -> (super::SystemRing<3>, super::SystemRing<3>) {
    let (mut a, mut b) = (ZeroSim::default(), ZeroSim::default());
    let (mut ring_a, mut ring_b) = (
        super::SystemRing::<3>::with_raw(),
        super::SystemRing::<3>::with_raw(),
    );
    for tick in 1..=ticks {
        if tick == split_at {
            a.zero = 0.0;
            b.zero = -0.0;
        }
        a.step(tick, recip_at);
        b.step(tick, recip_at);
        ring_a.record(tick, &a);
        ring_b.record(tick, &b);
    }
    (ring_a, ring_b)
}

#[test]
fn raw_ring_names_a_zero_sign_split_at_its_tick() {
    // The zero sign splits at 40; the reciprocal turns it into +inf / -inf at 55.
    let (ring_a, ring_b) = zero_pair(60, 40, Some(55));
    let (left, right) = (ring_a.window_rows(60, 60), ring_b.window_rows(60, 60));
    let canonical = super::diverged_systems(&ZERO_SYSTEMS, &ring_a.window(60, 60), &ring_b.window(60, 60));
    assert_eq!(
        canonical.list(),
        "motion@55",
        "the canonical rule hides the zero sign until 1/x"
    );
    let d = super::diagnose(&ZERO_SYSTEMS, &left, &right);
    assert_eq!(
        d.canonical, canonical,
        "the diagnosis keeps the canonical report as is"
    );
    assert_eq!(
        d.float_bits,
        vec![FloatBitsSplit {
            system: "motion",
            raw_tick: 40,
            canonical_tick: Some(55),
        }]
    );
    let raw = d.raw.as_ref().expect("both rings carry raw rows");
    assert_eq!(raw.list(), "motion@40");
    assert_eq!(raw.last_agreed, Some(39));
    let lines = super::diagnosis_lines(60, "a", "b", &d);
    assert_eq!(lines[..2], super::divergence_lines(60, "a", "b", &canonical)[..]);
    assert_eq!(
        lines[2],
        "lockstep desync raw motion tick 40 a vs b: zero sign or NaN payload split (canonical at tick 55)"
    );
    assert_eq!(lines.len(), 3);

    // With no reciprocal the canonical words never split; the raw ring still names tick 40.
    let (ring_a, ring_b) = zero_pair(60, 40, None);
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(60, 60),
        &ring_b.window_rows(60, 60),
    );
    assert!(d.canonical.systems.is_empty(), "canonical: never");
    assert_eq!(d.float_bits.len(), 1);
    assert_eq!(
        (d.float_bits[0].raw_tick, d.float_bits[0].canonical_tick),
        (40, None)
    );
    assert!(super::diagnosis_lines(60, "a", "b", &d)[1].ends_with("(canonical never)"));
}

#[test]
fn a_nan_payload_split_is_named_by_the_raw_ring_only() {
    let (mut a, mut b) = (ZeroSim::default(), ZeroSim::default());
    let (mut ring_a, mut ring_b) = (
        super::SystemRing::<3>::with_raw(),
        super::SystemRing::<3>::with_raw(),
    );
    for tick in 1..=20 {
        if tick == 7 {
            a.nan = f64::from_bits(0x7ff8_0000_0000_0001);
            b.nan = f64::from_bits(0xfff8_0000_0000_0002);
        }
        a.step(tick, None);
        b.step(tick, None);
        ring_a.record(tick, &a);
        ring_b.record(tick, &b);
    }
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(20, 20),
        &ring_b.window_rows(20, 20),
    );
    assert!(d.canonical.systems.is_empty());
    assert_eq!(d.float_bits.len(), 1);
    assert_eq!((d.float_bits[0].system, d.float_bits[0].raw_tick), ("economy", 7));
}

#[test]
fn the_raw_ring_leaves_the_canonical_rows_alone() {
    // The canonical words of a raw ring are the words of a plain ring (and of `system_hashes`): the
    // wire stays DO27's whatever the diagnose switch says.
    let mut state = boot(3);
    let mut plain = SystemRing::new();
    let mut raw = SystemRing::with_raw();
    assert!(!plain.raw_enabled());
    assert!(raw.raw_enabled());
    for tick in 1..=30u64 {
        state.tick = tick;
        plain.record(tick, &state);
        raw.record(tick, &state);
    }
    assert_eq!(plain.window(30, 30), raw.window(30, 30));
    let rows = raw.window_rows(30, 30);
    assert_eq!(rows.len(), 30);
    assert!(rows.iter().all(|row| row.raw.is_some() && !row.partial));
    assert!(plain.window_rows(30, 30).iter().all(|row| row.raw.is_none()));
    assert_eq!(rows[29].hashes, system_hashes(&state));
}

#[test]
fn raw_words_equal_canonical_words_without_a_signed_zero_or_nan() {
    let state = ZeroSim {
        tick: 4,
        zero: 0.0,
        inv: 2.5,
        pos: [3.0, -4.0],
        gold: 7,
        nan: -1.25,
    };
    let canonical = super::fold_system_hashes(&state, FloatRule::Canonical);
    let raw = super::fold_system_hashes(&state, FloatRule::Raw);
    assert_eq!(canonical.hashes, raw.hashes);
    assert_eq!(canonical.hashes, super::system_hashes(&state));
    let signed = ZeroSim { zero: -0.0, ..state };
    let raw_signed = super::fold_system_hashes(&signed, FloatRule::Raw).hashes;
    assert_eq!(ZERO_SYSTEMS.first_diff(&raw.hashes, &raw_signed), Some("motion"));
    assert_eq!(super::system_hashes(&signed), canonical.hashes);
    assert_eq!(Mix::new_raw(5).rule(), FloatRule::Raw);
    assert_eq!(Mix::new(5).rule(), FloatRule::Canonical);
}

#[test]
fn a_raw_ring_rewinds_and_scrolls_its_raw_rows_with_the_canonical_rows() {
    let mut ring = super::SystemRing::<3>::with_raw();
    let mut state = ZeroSim::default();
    for tick in 1..=300u64 {
        state.step(tick, None);
        ring.record(tick, &state);
    }
    assert_eq!(ring.len(), RING_TICKS);
    let rows = ring.window_rows(300, 10);
    assert_eq!(rows[0].tick, 291);
    assert!(
        rows.iter().all(|row| row.raw == Some(row.hashes)),
        "no signed zero or NaN"
    );
    // Rewind to 295 with a canonical-only row: the raw slot of that row is empty, the rest stay.
    ring.push(295, super::SystemHashes::<3>::default());
    let rows = ring.window_rows(300, 10);
    assert_eq!(rows.last().map(|row| row.tick), Some(295));
    assert_eq!(rows.last().and_then(|row| row.raw), None);
    assert!(rows[..rows.len() - 1].iter().all(|row| row.raw.is_some()));
}

/// A value whose `Serialize` fails half way when `fail` is set (as a user impl that hits a poisoned
/// lock or an unsupported state would).
#[derive(Clone, Debug, Default)]
struct Flaky {
    before: u32,
    fail: bool,
    after: u32,
}

impl serde::Serialize for Flaky {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeStruct};
        let mut st = serializer.serialize_struct("Flaky", 2)?;
        st.serialize_field("before", &self.before)?;
        if self.fail {
            return Err(S::Error::custom("cannot serialize this state"));
        }
        st.serialize_field("after", &self.after)?;
        st.end()
    }
}

#[derive(Clone, Debug, Default)]
struct FlakySim {
    flaky: Flaky,
    gold: u32,
}

impl SystemFold<2> for FlakySim {
    const SYSTEMS: Systems<2> = Systems::new(["flaky", "economy"]);
    fn fold_systems(&self, mix: &mut [Mix; 2]) {
        mix[0].fold(&self.flaky);
        mix[1].fold(&self.gold);
    }
}

#[test]
fn a_failing_serialize_marks_the_hash_partial() {
    let mut mix = Mix::new(1);
    mix.fold(&Flaky {
        before: 1,
        fail: true,
        after: 2,
    });
    assert_eq!(mix.errors(), 1, "the swallowed error is counted");
    let mut whole = Mix::new(1);
    whole.fold(&Flaky::default());
    assert_eq!(whole.errors(), 0);

    let mut sim = FlakySim::default();
    let folded = super::fold_system_hashes(&sim, FloatRule::Canonical);
    assert!(!folded.partial);
    sim.flaky.fail = true;
    let folded = super::fold_system_hashes(&sim, FloatRule::Canonical);
    assert!(folded.partial, "a failing Serialize marks the vector partial");

    // The ring keeps the flag per row; the diagnosis reports the first partial tick it compares.
    let mut ring_a = super::SystemRing::<2>::new();
    let mut ring_b = super::SystemRing::<2>::new();
    for tick in 1..=8u64 {
        sim.flaky.fail = tick >= 5;
        sim.gold = tick as u32;
        ring_a.record(tick, &sim);
        ring_b.record(tick, &sim);
    }
    assert_eq!(ring_a.partial_rows(), 4);
    let rows = ring_a.window_rows(8, 8);
    assert_eq!(
        rows.iter().map(|row| row.partial).collect::<Vec<_>>(),
        [false, false, false, false, true, true, true, true]
    );
    let d = super::diagnose(&FlakySim::SYSTEMS, &rows, &ring_b.window_rows(8, 8));
    assert_eq!(d.partial_from, Some(5));
    assert!(d.canonical.systems.is_empty());
    let lines = super::diagnosis_lines(8, "a", "b", &d);
    assert_eq!(
        lines.last().map(String::as_str),
        Some(
            "lockstep desync partial from tick 5: a Serialize failed while folding; those words cover part of the state"
        )
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a Serialize failed while folding the desync hash")]
fn system_hashes_debug_asserts_a_whole_fold() {
    let sim = FlakySim {
        flaky: Flaky {
            fail: true,
            ..Flaky::default()
        },
        gold: 0,
    };
    let _ = super::system_hashes(&sim);
}

/// SplitMix64 (a fixed, seeded test source; Steele, Lea and Flood 2014).
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// One flip layout's tally: for each (input bit, output bit) pair, how often the output bit changed.
struct Avalanche {
    collisions: u64,
    flips_per_bit: u64,
    changed: Vec<[u32; 64]>,
}

impl Avalanche {
    /// `|P(output bit changes) - 1/2|` per pair.
    fn bias(&self, input: usize, output: usize) -> f64 {
        (f64::from(self.changed[input][output]) / self.flips_per_bit as f64 - 0.5).abs()
    }
}

/// Fold `words` 64-bit words from a seeded source into a canonical mixer (tag 1); flip one bit of word
/// `at` (bit `i % 64` on sample `i`), fold again and compare the finished words. 2^20 samples.
fn avalanche(words: usize, at: usize, seed: u64) -> Avalanche {
    const SAMPLES: u64 = 1 << 20;
    let mut rng = seed;
    let mut out = Avalanche {
        collisions: 0,
        flips_per_bit: SAMPLES / 64,
        changed: vec![[0u32; 64]; 64],
    };
    let mut input = [0u64; 4];
    for i in 0..SAMPLES {
        for w in input.iter_mut().take(words) {
            *w = splitmix(&mut rng);
        }
        let bit = (i % 64) as usize;
        let fold = |flip: bool| {
            let mut mix = Mix::new(1);
            for (k, w) in input.iter().take(words).enumerate() {
                mix.u64(if flip && k == at { *w ^ (1u64 << bit) } else { *w });
            }
            mix.finish()
        };
        let diff = fold(false) ^ fold(true);
        if diff == 0 {
            out.collisions += 1;
        }
        let row = &mut out.changed[bit];
        for (o, count) in row.iter_mut().enumerate() {
            *count += ((diff >> o) & 1) as u32;
        }
    }
    out
}

/// The strict avalanche criterion (Webster and Tavares 1985) measured on the one-multiply mixer, with
/// fixed seeds: 2 x 2^20 single-bit flips. Every step of the mix is a bijection of the state, so a
/// one-word difference can never collide; the count proves it on these samples. The bias bound is 6
/// standard deviations of a fair coin over 16384 flips per pair (0.5 / 128 x 6 = 0.0234).
#[test]
fn mixer_has_no_collisions_over_seeded_single_bit_flips() {
    const BOUND: f64 = 6.0 * 0.5 / 128.0;
    // A flip followed by more words (the usual case: a field in the middle of a system's rows).
    let mid = avalanche(2, 0, 0x5eed_0001);
    assert_eq!(mid.collisions, 0);
    let mut worst = 0.0f64;
    for input in 0..64 {
        for output in 0..64 {
            let bias = mid.bias(input, output);
            if bias > worst {
                worst = bias;
            }
        }
    }
    assert!(worst <= BOUND, "worst bias {worst} over the bound {BOUND}");

    // A flip in the last word before the finish: no collision either, but the top input bits reach
    // only part of the output (one multiply, then the finish). Pin that shape: weak pairs only from
    // the top byte, at most 128 of the 4096 pairs, and a mean bias close to a fair coin's.
    let last = avalanche(1, 0, 0x5eed_0002);
    assert_eq!(last.collisions, 0);
    let mut weak = 0usize;
    let mut total = 0.0f64;
    for input in 0..64 {
        for output in 0..64 {
            let bias = last.bias(input, output);
            total += bias;
            if bias > BOUND {
                weak += 1;
                assert!(
                    input >= 56,
                    "a weak pair below the top byte: input {input} output {output} bias {bias}"
                );
            }
        }
    }
    let mean = total / 4096.0;
    assert!(weak <= 128, "{weak} weak pairs");
    assert!(mean <= 0.01, "mean bias {mean}");
}

// ---------------------------------------------------------------------------------------------------
// ADR-8621 (Regia's own tests): the verdict, partial ticks, the ring's saved state and the mixer's
// pinned blind spots.
// ---------------------------------------------------------------------------------------------------

fn flaky_sim(fail: bool, after: u32) -> FlakySim {
    FlakySim {
        flaky: Flaky {
            before: 1,
            fail,
            after,
        },
        gold: 1,
    }
}

#[test]
fn a_partial_tail_mismatch_is_not_agreement() {
    // Both peers fail the same `Serialize` at tick 5; the field after the failure differs, so the
    // words agree while the states do not.
    let (mut ring_a, mut ring_b) = (super::SystemRing::<2>::new(), super::SystemRing::<2>::new());
    for tick in 1..=4u64 {
        ring_a.record(tick, &flaky_sim(false, 9));
        ring_b.record(tick, &flaky_sim(false, 9));
    }
    ring_a.record(5, &flaky_sim(true, 9));
    ring_b.record(5, &flaky_sim(true, 0));
    let d = super::diagnose(
        &FlakySim::SYSTEMS,
        &ring_a.window_rows(5, 5),
        &ring_b.window_rows(5, 5),
    );
    assert!(
        d.canonical.systems.is_empty(),
        "the tail after the failure was never hashed"
    );
    assert_eq!(d.partial_from, Some(5));
    assert_eq!(
        d.canonical.last_agreed,
        Some(4),
        "a partial tick never counts as agreement"
    );
    // A partial first row leaves nothing agreed.
    let d = super::diagnose(
        &FlakySim::SYSTEMS,
        &ring_a.window_rows(5, 1),
        &ring_b.window_rows(5, 1),
    );
    assert_eq!((d.partial_from, d.canonical.last_agreed), (Some(5), None));
}

#[test]
fn the_verdict_is_same_only_when_every_view_agrees() {
    let lines = |d: &Diagnosis| super::diagnosis_lines(20, "a", "b", d);

    // Agreement: every compared row whole, every word equal.
    let (ring_a, _) = zero_pair(20, 5, None);
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(20, 20),
        &ring_a.window_rows(20, 20),
    );
    assert_eq!(d.verdict(), Verdict::Same);
    assert!(d.same());
    assert_eq!(
        lines(&d)[0],
        "lockstep desync systems tick 20 a vs b: no system differs in the 20 shared ticks"
    );

    // A zero sign that never changes a value: the canonical words agree, the raw words do not.
    let (ring_a, ring_b) = zero_pair(20, 5, None);
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(20, 20),
        &ring_b.window_rows(20, 20),
    );
    assert!(d.canonical.systems.is_empty());
    assert_eq!(d.verdict(), Verdict::FloatBits);
    assert!(!d.same());
    let out = lines(&d);
    assert_eq!(
        out[0],
        "lockstep desync systems tick 20 a vs b: raw words split first motion at tick 5 (zero sign or NaN payload; the canonical words agree in the 20 shared ticks)"
    );
    assert!(out[1].starts_with("lockstep desync raw motion tick 5"), "{out:?}");

    // The zero sign turns into a canonical split at 9: the canonical view answers first.
    let (ring_a, ring_b) = zero_pair(20, 5, Some(9));
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(20, 20),
        &ring_b.window_rows(20, 20),
    );
    assert_eq!(d.verdict(), Verdict::Canonical);
    assert_eq!(
        lines(&d)[..2],
        super::divergence_lines(20, "a", "b", &d.canonical)[..]
    );

    // No shared tick: nothing was compared, so nothing agreed.
    let d = super::diagnose(
        &ZERO_SYSTEMS,
        &ring_a.window_rows(8, 8),
        &ring_a.window_rows(20, 5),
    );
    assert_eq!(d.canonical.compared, 0);
    assert_eq!(d.verdict(), Verdict::NoOverlap);
    assert!(!d.same());
    assert_eq!(
        lines(&d)[0],
        "lockstep desync systems tick 20 a vs b: not agreed: no shared tick (0 compared)"
    );

    // A partial row whose words agree.
    let (mut pa, mut pb) = (super::SystemRing::<2>::new(), super::SystemRing::<2>::new());
    pa.record(1, &flaky_sim(false, 9));
    pb.record(1, &flaky_sim(false, 9));
    pa.record(2, &flaky_sim(true, 9));
    pb.record(2, &flaky_sim(true, 0));
    let d = super::diagnose(&FlakySim::SYSTEMS, &pa.window_rows(2, 2), &pb.window_rows(2, 2));
    assert_eq!(d.verdict(), Verdict::Partial);
    assert!(!d.same());
    let out = super::diagnosis_lines(2, "a", "b", &d);
    assert_eq!(
        out[0],
        "lockstep desync systems tick 2 a vs b: not agreed: partial rows from tick 2 (the words agree in the 2 shared ticks, but a Serialize failed)"
    );
    assert_eq!(
        out.last().map(String::as_str),
        Some(
            "lockstep desync partial from tick 2: a Serialize failed while folding; those words cover part of the state"
        )
    );

    let names: Vec<&str> = [
        Verdict::Same,
        Verdict::Canonical,
        Verdict::FloatBits,
        Verdict::Partial,
        Verdict::NoOverlap,
    ]
    .iter()
    .map(|v| v.as_str())
    .collect();
    assert_eq!(
        names,
        ["same", "canonical", "float_bits", "partial", "no_overlap"]
    );
}

#[test]
fn a_loaded_ring_keeps_one_flag_and_one_raw_slot_per_row() {
    let mut ring = super::SystemRing::<3>::with_raw();
    let mut sim = ZeroSim::default();
    for tick in 1..=4u64 {
        sim.step(tick, None);
        ring.record(tick, &sim);
    }
    let bytes = crate::state::snapshot(&ring, 4).expect("save");
    let (_, mut loaded): (u64, super::SystemRing<3>) = crate::state::restore(&bytes).expect("load");
    assert!(loaded.raw_enabled(), "a raw ring loads as a raw ring");
    assert_eq!(
        loaded.window(4, 4),
        ring.window(4, 4),
        "the canonical rows round-trip"
    );
    // Raw words and partial flags are diagnosis state: a loaded row is whole and has no raw words.
    assert!(
        loaded
            .window_rows(4, 4)
            .iter()
            .all(|row| !row.partial && row.raw.is_none())
    );

    // Rows recorded after the load keep their flags and raw words on their own ticks.
    let words = super::system_hashes(&sim);
    loaded.push_row(RingRow {
        tick: 5,
        hashes: words,
        raw: Some(words),
        partial: true,
    });
    sim.step(6, None);
    loaded.record(6, &sim);
    let rows = loaded.window_rows(6, 6);
    assert_eq!(
        rows.iter().map(|row| row.tick).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        rows.iter().map(|row| row.partial).collect::<Vec<_>>(),
        [false, false, false, false, true, false]
    );
    assert_eq!(
        rows.iter().map(|row| row.raw.is_some()).collect::<Vec<_>>(),
        [false, false, false, false, true, true]
    );
    assert_eq!(rows[5].hashes, super::system_hashes(&sim));

    // A rewind across the loaded rows drops their slots with them.
    loaded.push(3, words);
    let rows = loaded.window_rows(6, 6);
    assert_eq!(rows.iter().map(|row| row.tick).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(rows.iter().all(|row| !row.partial && row.raw.is_none()));

    // A plain ring loads as a plain ring.
    let mut plain = super::SystemRing::<3>::new();
    plain.record(1, &sim);
    let (_, plain): (u64, super::SystemRing<3>) =
        crate::state::restore(&crate::state::snapshot(&plain, 1).expect("save")).expect("load");
    assert!(!plain.raw_enabled());
    assert_eq!(plain.len(), 1);
}

#[test]
fn clear_empties_a_raw_ring_and_keeps_it_raw() {
    let mut ring = super::SystemRing::<3>::with_raw();
    let mut sim = ZeroSim::default();
    for tick in 1..=5u64 {
        sim.step(tick, None);
        ring.record(tick, &sim);
    }
    ring.clear();
    assert!(ring.is_empty() && ring.raw_enabled());
    ring.record(2, &sim);
    let rows = ring.window_rows(5, 5);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].tick, rows[0].raw.is_some(), rows[0].partial),
        (2, true, false)
    );
}

/// ADR-8621 keeps Direct Order 27's encoding (ADR-8012 / ADR-8047): `Mix::fold` mixes field values
/// only, with no field names, type names or field counts. These pins make the two blind spots the
/// review found visible: a change of the encoding fails here (and moves `PINNED_MIX_WORDS`) on
/// purpose, never silently.
#[test]
fn the_mixer_does_not_see_unit_fields_or_skipped_option_slots() {
    use serde::Serialize;

    fn word<T: Serialize>(value: &T) -> u64 {
        let mut mix = Mix::new(1);
        mix.fold(value);
        mix.finish()
    }

    #[derive(Serialize)]
    struct Marker;
    #[derive(Serialize)]
    struct Body {
        hp: u32,
    }
    #[derive(Serialize)]
    struct BodyMarked {
        hp: u32,
        marker: Marker,
    }
    assert_eq!(
        word(&Body { hp: 7 }),
        word(&BodyMarked {
            hp: 7,
            marker: Marker
        }),
        "a unit field writes nothing"
    );

    #[derive(Serialize)]
    struct Gates {
        #[serde(skip_serializing_if = "Option::is_none")]
        left: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        right: Option<u32>,
    }
    assert_eq!(
        word(&Gates {
            left: Some(5),
            right: None
        }),
        word(&Gates {
            left: None,
            right: Some(5)
        }),
        "a skipped slot writes nothing, and field names are not mixed"
    );

    // The controls: an option that is not skipped writes its `None`, so the two states differ, and
    // a value-carrying field changes the word.
    #[derive(Serialize)]
    struct OpenGates {
        left: Option<u32>,
        right: Option<u32>,
    }
    assert_ne!(
        word(&OpenGates {
            left: Some(5),
            right: None
        }),
        word(&OpenGates {
            left: None,
            right: Some(5)
        })
    );
    #[derive(Serialize)]
    struct BodyArmored {
        hp: u32,
        armor: u32,
    }
    assert_ne!(word(&Body { hp: 7 }), word(&BodyArmored { hp: 7, armor: 0 }));
}

/// Review proof for finding C (`sprint-det-desync-hash-hardening-review`).
///
/// The agent refuses a repeated tick in `against`. [`super::diverged_systems`] still keeps the
/// first row at that tick and never reads the second, so a mismatch that lives only in the second
/// row is not named. This is the review's smallest test, unchanged.
#[test]
fn review_proof_a_second_row_at_the_same_tick_is_compared() {
    const NAMES: super::Systems<1> = super::Systems::new(["motion"]);
    let left = [super::TraceRow {
        tick: 5,
        hashes: super::SystemHashes { words: [1] },
    }];
    let right = [
        super::TraceRow {
            tick: 5,
            hashes: super::SystemHashes { words: [1] },
        },
        super::TraceRow {
            tick: 5,
            hashes: super::SystemHashes { words: [2] },
        },
    ];
    let report = super::diverged_systems(&NAMES, &left, &right);
    assert_eq!(
        report.list(),
        "motion@5",
        "the second row at tick 5 was dropped (systems {:?}, compared {})",
        report.systems,
        report.compared
    );
}

// ---------------------------------------------------------------------------------------------------
// Every row at a tick takes part in the join (ADR-8724)
// ---------------------------------------------------------------------------------------------------

const PAIR: super::Systems<2> = super::Systems::new(["motion", "economy"]);

fn row2(tick: u64, motion: u64, economy: u64) -> super::TraceRow<2> {
    super::TraceRow {
        tick,
        hashes: super::SystemHashes {
            words: [motion, economy],
        },
    }
}

fn unmatched(side: super::RowSide, tick: u64, index: usize, tick_shared: bool) -> super::UnmatchedRow {
    super::UnmatchedRow {
        side,
        tick,
        index,
        tick_shared,
    }
}

#[test]
fn a_second_left_row_at_a_tick_is_compared_in_either_order() {
    // The split lives only in the left side's second row at tick 5; the first left row agrees.
    let right = [row2(4, 1, 1), row2(5, 1, 1), row2(6, 1, 1)];
    let ordered = [row2(4, 1, 1), row2(5, 1, 1), row2(5, 2, 1), row2(6, 1, 1)];
    let swapped = [row2(6, 1, 1), row2(5, 2, 1), row2(5, 1, 1), row2(4, 1, 1)];
    for left in [&ordered[..], &swapped[..]] {
        let report = super::diverged_systems(&PAIR, left, &right);
        assert_eq!(report.list(), "motion@5", "{left:?}");
        assert_eq!(report.compared, 3);
        assert_eq!(
            report.last_agreed,
            Some(4),
            "tick 5 holds a split row, so it is not agreement"
        );
        assert_eq!(report.surplus_ticks(), vec![5]);
        assert_eq!(report.unmatched.len(), 1);
        assert!(report.unmatched[0].tick_shared);
        assert_eq!(report.unmatched[0].side, super::RowSide::Left);
    }
}

#[test]
fn only_the_system_that_differs_in_the_extra_row_is_named() {
    let left = [row2(5, 1, 1)];
    let right = [row2(5, 1, 1), row2(5, 1, 9), row2(6, 3, 1)];
    let report = super::diverged_systems(&PAIR, &left, &right);
    assert_eq!(report.list(), "economy@5");
    assert_eq!(report.compared, 1, "tick 6 is on the right only");
    assert_eq!(report.last_agreed, None);
    assert_eq!(
        report.unmatched,
        vec![
            unmatched(super::RowSide::Right, 5, 1, true),
            unmatched(super::RowSide::Right, 6, 2, false),
        ]
    );
}

#[test]
fn two_different_rows_on_both_sides_are_not_agreement() {
    // The same two vectors on each side at tick 5: no pairing makes every pair equal.
    let left = [row2(5, 1, 1), row2(5, 2, 1)];
    let right = [row2(5, 2, 1), row2(5, 1, 1)];
    let report = super::diverged_systems(&PAIR, &left, &right);
    assert_eq!(report.list(), "motion@5");
    assert_eq!(report.last_agreed, None);
    assert!(
        report.unmatched.is_empty(),
        "two rows a side pair up, none is surplus"
    );
}

#[test]
fn an_identical_duplicate_row_agrees_and_is_listed() {
    let left = [row2(4, 1, 1), row2(5, 1, 1), row2(5, 1, 1)];
    let right = [row2(4, 1, 1), row2(5, 1, 1)];
    let report = super::diverged_systems(&PAIR, &left, &right);
    assert!(report.systems.is_empty());
    assert_eq!(report.last_agreed, Some(5));
    assert_eq!(
        report.unmatched,
        vec![unmatched(super::RowSide::Left, 5, 2, true)]
    );
}

#[test]
fn unmatched_rows_come_out_by_tick_then_side_then_position() {
    let left = [row2(9, 1, 1), row2(2, 1, 1), row2(7, 1, 1), row2(7, 1, 1)];
    let right = [row2(7, 1, 1), row2(3, 1, 1), row2(9, 1, 1), row2(1, 1, 1)];
    let report = super::diverged_systems(&PAIR, &left, &right);
    assert_eq!(
        report.unmatched,
        vec![
            unmatched(super::RowSide::Right, 1, 3, false),
            unmatched(super::RowSide::Left, 2, 1, false),
            unmatched(super::RowSide::Right, 3, 1, false),
            unmatched(super::RowSide::Left, 7, 3, true),
        ]
    );
    assert_eq!((report.compared, report.last_agreed), (2, Some(9)));
    // Same answer for the mirrored call, sides swapped.
    let mirrored = super::diverged_systems(&PAIR, &right, &left);
    assert_eq!((mirrored.compared, mirrored.last_agreed), (2, Some(9)));
    assert_eq!(mirrored.unmatched.len(), 4);
}

#[test]
fn the_join_does_not_depend_on_row_order() {
    // A small deterministic shuffle: every rotation of both slices gives the same systems,
    // compared count, last agreed tick and surplus ticks.
    let left = [
        row2(1, 1, 1),
        row2(2, 1, 1),
        row2(3, 1, 1),
        row2(3, 1, 4),
        row2(4, 3, 4),
    ];
    let right = [row2(1, 1, 1), row2(2, 1, 1), row2(3, 1, 1), row2(4, 2, 1)];
    let base = super::diverged_systems(&PAIR, &left, &right);
    assert_eq!(base.list(), "economy@3,motion@4");
    assert_eq!(base.last_agreed, Some(2));
    for l in 0..left.len() {
        for r in 0..right.len() {
            let (mut a, mut b) = (left.to_vec(), right.to_vec());
            a.rotate_left(l);
            b.rotate_left(r);
            let report = super::diverged_systems(&PAIR, &a, &b);
            assert_eq!(
                (
                    report.list(),
                    report.compared,
                    report.last_agreed,
                    report.surplus_ticks()
                ),
                (base.list(), base.compared, base.last_agreed, base.surplus_ticks()),
                "rotation {l} {r}"
            );
        }
    }
}

fn ring2(tick: u64, words: [u64; 2], raw: Option<[u64; 2]>, partial: bool) -> super::RingRow<2> {
    super::RingRow {
        tick,
        hashes: super::SystemHashes { words },
        raw: raw.map(|words| super::SystemHashes { words }),
        partial,
    }
}

#[test]
fn diagnose_reads_every_row_at_a_tick() {
    // A partial second row at tick 5 on the right: the words agree, but tick 5 is not agreement.
    let left = [ring2(4, [1, 1], None, false), ring2(5, [1, 1], None, false)];
    let right = [
        ring2(4, [1, 1], None, false),
        ring2(5, [1, 1], None, false),
        ring2(5, [1, 1], None, true),
    ];
    let d = super::diagnose(&PAIR, &left, &right);
    assert!(d.canonical.systems.is_empty());
    assert_eq!(d.partial_from, Some(5));
    assert_eq!(d.canonical.last_agreed, Some(4));
    assert_eq!(d.verdict(), super::Verdict::Partial);

    // A raw-only split in the right side's second row at tick 5 (the canonical words agree).
    let left = [
        ring2(4, [1, 1], Some([1, 1]), false),
        ring2(5, [1, 1], Some([1, 1]), false),
    ];
    let right = [
        ring2(4, [1, 1], Some([1, 1]), false),
        ring2(5, [1, 1], Some([1, 1]), false),
        ring2(5, [1, 1], Some([7, 1]), false),
    ];
    let d = super::diagnose(&PAIR, &left, &right);
    assert!(d.canonical.systems.is_empty());
    assert_eq!(
        d.float_bits,
        vec![super::FloatBitsSplit {
            system: "motion",
            raw_tick: 5,
            canonical_tick: None,
        }]
    );
    let raw = d.raw.as_ref().expect("both sides carry raw rows");
    assert_eq!(raw.last_agreed, Some(4));
    assert_eq!(
        raw.unmatched,
        vec![unmatched(super::RowSide::Right, 5, 2, true)],
        "positions are the caller's"
    );
    let lines = super::diagnosis_lines(5, "a", "b", &d);
    assert_eq!(
        lines.last().map(String::as_str),
        Some(
            "lockstep desync rows a vs b: more rows on one side at 1 ticks from tick 5 (0 surplus a, 1 surplus b); every row there was compared"
        )
    );

    // One raw row on one side and none on the other at the shared tick: no raw report.
    let left = [ring2(5, [1, 1], Some([1, 1]), false)];
    let right = [
        ring2(5, [1, 1], None, false),
        ring2(6, [1, 1], Some([1, 1]), false),
    ];
    let d = super::diagnose(&PAIR, &left, &right);
    assert!(d.raw.is_none());
    assert_eq!(d.verdict(), super::Verdict::Same);
}

#[test]
fn one_row_a_tick_keeps_the_old_lines() {
    // The ring never holds two rows at a tick; with one row a tick the lines are unchanged.
    let left = [row2(4, 1, 1), row2(5, 2, 1)];
    let right = [row2(3, 1, 1), row2(4, 1, 1), row2(5, 1, 1)];
    let report = super::diverged_systems(&PAIR, &left, &right);
    assert!(report.surplus_ticks().is_empty());
    let d = super::diagnose(
        &PAIR,
        &left.map(|row| super::RingRow::new(row.tick, row.hashes)),
        &right.map(|row| super::RingRow::new(row.tick, row.hashes)),
    );
    assert_eq!(
        super::diagnosis_lines(5, "a", "b", &d),
        super::divergence_lines(5, "a", "b", &report)
    );
}
