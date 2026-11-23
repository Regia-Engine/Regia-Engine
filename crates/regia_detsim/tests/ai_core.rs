// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/ai_core.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (plan row S-09). `rng_streams_diverge_by_seed_and_entity` and
// `scheduler_spreads_decisions_across_units` keep DO27's assertions; `pipeline_is_deterministic_over_ticks`
// keeps its decision assertions and drives the scheduler through a test catalogue (the sightings board it also
// compared is plan row S-10's); DO27's wall-clock cost bench became the counter test
// `ai_work_per_tick_is_bounded_for_60_units` (no wall-clock budgets in tests); `option_repertoire_matches_level`
// needs the difficulty profiles of plan row S-11 and is ported there. `ai_schedule_slices_by_entity_id` and the
// bit pins are new.
//
// Portions copied from Direct Order 27 (ir-sim/src/state.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 30126-30167, plan row S-09 re-copy, ADR-8484). `id_index_matches_a_scan_after_a_middle_remove`
// keeps the steps of DO27's id-index test over a plain id array. The roster tests, the tick chain and the cost
// counters are new.

//! The deterministic AI core (plan row S-09, ADR-8152): the time-sliced scheduler, `AiRng` and the IAUS scorer
//! with the product rule.

use regia_detsim::ai::{
    AiCadence, AiCore, AiOption, AiRng, AiSchedule, ArgMax, Consideration, ConsiderationId, CurveKind,
    Inputs, OptionId, ResponseCurve, score, score_options,
};
use regia_detsim::float::det_clamp;
use regia_detsim::math;

/// A unit of the test world: an entity id, a team and an XZ position.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Unit {
    id: u32,
    team: u8,
    pos: [f32; 2],
}

impl regia_detsim::ai::AiAgent for Unit {
    fn entity_id(&self) -> u32 {
        self.id
    }
}

/// DO27's test layout: two teams of five, 12 m apart on a line, 120 m between the lines. The units drift with
/// the tick so the inputs change over a run.
fn make_world(tick: u64) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut id = 1u32;
    let drift = (tick % 240) as f32 * 0.25;
    for team in 1..=2u8 {
        for i in 0..5u32 {
            let x = (i as f32 - 2.0) * 12.0;
            let z = if team == 1 { -60.0 + drift } else { 60.0 - drift };
            units.push(Unit {
                id,
                team,
                pos: [x, z],
            });
            id += 1;
        }
    }
    units
}

/// A small engine-neutral catalogue: push into a range band, hold while healthy, retreat when hurt and
/// outnumbered, strafe at mid range. Ammo multiplies into push (the product rule: a dry unit never pushes).
fn catalogue() -> Vec<AiOption> {
    vec![
        AiOption::new(
            OptionId::Push,
            vec![
                Consideration::add(
                    ConsiderationId::DistanceToThreatFrac,
                    ResponseCurve::linear(1.0),
                    1.0,
                ),
                Consideration::mult(ConsiderationId::AmmoPct, ResponseCurve::plateau(0.2)),
            ],
        ),
        AiOption::new(
            OptionId::Hold,
            vec![Consideration::add(
                ConsiderationId::HullPct,
                ResponseCurve::logistic(10.0, 0.5),
                0.6,
            )],
        ),
        AiOption::new(
            OptionId::Retreat,
            vec![
                Consideration::add(
                    ConsiderationId::HullPct,
                    ResponseCurve::inverse_logistic(12.0, 0.3),
                    1.0,
                ),
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::inverse_logistic(8.0, 0.5),
                    1.0,
                ),
            ],
        ),
        AiOption::new(
            OptionId::Strafe,
            vec![Consideration::add(
                ConsiderationId::RangeBandFrac,
                ResponseCurve::band(0.5, 0.2),
                1.0,
            )],
        ),
    ]
}

/// The inputs of one unit: distance to the nearest enemy (XZ, through `math::sqrt`), the range-band fraction of a
/// 0-150 m band, hull and ammo drawn from the unit's per-tick stream.
fn inputs_for(unit: &Unit, world: &[Unit], rng: &mut AiRng) -> Inputs {
    let mut nearest = f32::MAX;
    for other in world.iter().filter(|o| o.team != unit.team) {
        let dx = other.pos[0] - unit.pos[0];
        let dz = other.pos[1] - unit.pos[1];
        let d = math::sqrt(dx * dx + dz * dz);
        if d < nearest {
            nearest = d;
        }
    }
    Inputs {
        hull_pct: rng.next_f32(),
        ammo_pct: rng.next_f32(),
        distance_to_threat_frac: det_clamp(nearest / 300.0, 0.0, 1.0),
        range_band_frac: det_clamp(nearest / 150.0, 0.0, 1.0),
        local_force_ratio: 0.5,
        ..Inputs::default()
    }
}

fn run_tick(core: &mut AiCore<ArgMax>, tick: u64, world: &[Unit], catalogue: &[AiOption]) -> u32 {
    let report = core.decide(tick, world, |unit, rng, out| {
        let inputs = inputs_for(unit, world, rng);
        score_options(catalogue, &inputs, |_| true, |_| 1.0, out);
    });
    report.due
}

#[test]
fn ai_schedule_slices_by_entity_id() {
    // 60 units handed over in a scrambled order. Each tick the scorer runs for exactly the units whose
    // decision phase (entity id mod the cadence) is the tick's, in ascending entity-id order, whatever the
    // order of the slice; over one cadence window every unit is scored exactly once.
    let mut units: Vec<Unit> = (1..=60u32)
        .map(|id| Unit {
            id,
            team: 1 + (id % 2) as u8,
            pos: [0.0, 0.0],
        })
        .collect();
    units.reverse();
    units.swap(3, 41);
    units.swap(10, 57);
    let mut core = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    let mut scored = [0u32; 61];
    for tick in 0..AiSchedule::DECISION_TICKS as u64 * 3 {
        let mut called = Vec::new();
        let report = core.decide(tick, &units, |unit, _rng, out| {
            called.push(unit.id);
            out.push((OptionId::Hold, 0.5));
        });
        let expected: Vec<u32> = (1..=60u32).filter(|id| u64::from(id % 10) == tick % 10).collect();
        assert_eq!(
            called, expected,
            "tick {tick}: the slice is the ids whose phase is the tick's, ascending"
        );
        assert_eq!(
            report.due, 6,
            "tick {tick}: 60 units over a 10-tick cadence decide 6 per tick"
        );
        assert_eq!(
            report.scored_options, 6,
            "tick {tick}: one option scored per due unit"
        );
        for id in called {
            scored[id as usize] += 1;
            assert_eq!(core.decision_for(id), Some(OptionId::Hold));
        }
        if tick % 10 == 9 {
            let window = tick / 10 + 1;
            assert!(
                scored[1..].iter().all(|n| u64::from(*n) == window),
                "after window {window} every unit decided exactly {window} times: {:?}",
                &scored[1..]
            );
        }
    }

    // The cadence is a setting: four ticks slice 60 units into 15 per tick, and a sight cadence of six
    // gives each id its own sight phase.
    let cadence = AiCadence::new(4, 6);
    let mut core = AiCore::new(1, cadence.clone(), ArgMax);
    for tick in 0..8u64 {
        let mut called = Vec::new();
        core.decide(tick, &units, |unit, _rng, out| {
            called.push(unit.id);
            out.push((OptionId::Push, 1.0));
        });
        let expected: Vec<u32> = (1..=60u32).filter(|id| u64::from(id % 4) == tick % 4).collect();
        assert_eq!(called, expected, "tick {tick} at cadence 4");
    }
    assert!(cadence.due_to_sight(13, 7) && !cadence.due_to_sight(12, 7));
    // A zero cadence is clamped to every tick, never a division by zero.
    assert_eq!(AiCadence::new(0, 0), AiCadence::new(1, 1));
}

#[test]
fn pipeline_is_deterministic_over_ticks() {
    let seed = 0xD027_A1C0;
    let catalogue = catalogue();
    let mut a = AiCore::new(seed, AiCadence::default(), ArgMax);
    let mut b = AiCore::new(seed, AiCadence::default(), ArgMax);
    for tick in 0..120u64 {
        let world = make_world(tick);
        run_tick(&mut a, tick, &world, &catalogue);
        run_tick(&mut b, tick, &world, &catalogue);
        assert_eq!(a.decisions(), b.decisions(), "decisions drifted at tick {tick}");
    }
    // The framework must actually commit decisions for the AI units.
    assert!(!a.decisions().is_empty(), "no decision was ever committed");
    assert_eq!(a.decisions().len(), 10, "every unit decided in 120 ticks");
    // Another world seed draws other hull and ammo values, so somewhere the committed options differ.
    let mut c = AiCore::new(seed ^ 1, AiCadence::default(), ArgMax);
    for tick in 0..120u64 {
        run_tick(&mut c, tick, &make_world(tick), &catalogue);
    }
    assert_ne!(
        a.decisions(),
        c.decisions(),
        "the world seed reaches the decisions"
    );
}

#[test]
fn rng_streams_diverge_by_seed_and_entity() {
    // Same entity, different world seeds → different streams.
    assert_ne!(
        AiRng::unit_stream(1, 7).next_u64(),
        AiRng::unit_stream(2, 7).next_u64()
    );
    // Same seed, different entities → different streams.
    assert_ne!(
        AiRng::unit_stream(9, 1).next_u64(),
        AiRng::unit_stream(9, 2).next_u64()
    );
    // A zero mix never freezes the generator.
    assert_ne!(AiRng::mix(0, 0), 0);
}

#[test]
fn tick_stream_is_the_formula_the_sim_layers_use() {
    // DO27's motor, combat and mode-bot layers draw `AiRng::new(mix(world_seed ^ tick * golden, id))`;
    // `tick_stream` is that formula, so the later ports keep their bits.
    for (seed, tick, id) in [
        (7u64, 0u64, 1u32),
        (0xD027, 119, 42),
        (u64::MAX, u64::MAX, u32::MAX),
    ] {
        let mut by_hand = AiRng::new(AiRng::mix(
            seed ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            u64::from(id),
        ));
        let mut stream = AiRng::tick_stream(seed, tick, id);
        for _ in 0..16 {
            assert_eq!(stream.next_u64(), by_hand.next_u64());
        }
    }
    // Two ticks of one unit are different streams.
    assert_ne!(
        AiRng::tick_stream(5, 1, 3).next_u64(),
        AiRng::tick_stream(5, 2, 3).next_u64()
    );
}

#[test]
fn scheduler_spreads_decisions_across_units() {
    // No two units with different phases decide on the same tick; over a full DECISION_TICKS window every
    // unit decides exactly once.
    let catalogue = catalogue();
    let mut decided_once = [false; 10];
    let mut core = AiCore::new(1, AiCadence::default(), ArgMax);
    for tick in 0..AiSchedule::DECISION_TICKS as u64 {
        let world = make_world(tick);
        run_tick(&mut core, tick, &world, &catalogue);
        // After the run, whichever units were due this tick have a decision.
        for u in &world {
            if AiSchedule::due_to_decide(tick, u.id) {
                assert!(
                    core.decision_for(u.id).is_some(),
                    "unit {} was due but has no decision",
                    u.id
                );
                decided_once[(u.id - 1) as usize] = true;
            }
        }
    }
    assert!(
        decided_once.iter().all(|d| *d),
        "not every unit decided in a window"
    );
}

#[test]
fn ai_work_per_tick_is_bounded_for_60_units() {
    // DO27 timed this case (0.5 ms per 60 units in release); a test here counts the work instead: per tick
    // at most ceil(60 / DECISION_TICKS) units re-score, each over the catalogue once.
    let catalogue = catalogue();
    let mut core = AiCore::new(99, AiCadence::default(), ArgMax);
    let mut units = Vec::new();
    let mut id = 1u32;
    for team in 1..=2u8 {
        for i in 0..30u32 {
            let x = (i as f32 - 15.0) * 8.0;
            let z = if team == 1 { -120.0 } else { 120.0 };
            units.push(Unit {
                id,
                team,
                pos: [x, z],
            });
            id += 1;
        }
    }
    let per_tick = 60u32.div_ceil(AiSchedule::DECISION_TICKS);
    let mut total = 0u32;
    for tick in 0..200u64 {
        let report = core.decide(tick, &units, |unit, rng, out| {
            let inputs = inputs_for(unit, &units, rng);
            score_options(&catalogue, &inputs, |_| true, |_| 1.0, out);
        });
        assert!(
            report.due <= per_tick,
            "tick {tick}: {} units re-scored",
            report.due
        );
        assert_eq!(report.scored_options, report.due * catalogue.len() as u32);
        total += report.due;
    }
    assert_eq!(total, 60 * 20, "200 ticks are 20 windows of 60 decisions");
}

#[test]
fn product_rule_zeroes_an_option_and_the_filter_and_bias_apply() {
    let push = AiOption::new(
        OptionId::Push,
        vec![
            Consideration::add(ConsiderationId::HullPct, ResponseCurve::linear(1.0), 1.0),
            Consideration::mult(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0)),
        ],
    );
    let hold = AiOption::new(
        OptionId::Hold,
        vec![Consideration::add(
            ConsiderationId::HullPct,
            ResponseCurve::linear(1.0),
            1.0,
        )],
    );
    let dry = Inputs {
        hull_pct: 1.0,
        ammo_pct: 0.0,
        ..Inputs::default()
    };
    assert_eq!(score(&push, &dry), 0.0, "a dry unit never pushes");
    let catalogue = [push, hold];
    let mut out = Vec::new();
    score_options(
        &catalogue,
        &dry,
        |_| true,
        |id| if id == OptionId::Hold { 0.5 } else { 1.0 },
        &mut out,
    );
    assert_eq!(out, vec![(OptionId::Push, 0.0), (OptionId::Hold, 0.5)]);
    // The filter drops an option (a repertoire mask plugs in here), and the biased score is clamped to 0..1.
    out.clear();
    score_options(&catalogue, &dry, |id| id != OptionId::Push, |_| 3.0, &mut out);
    assert_eq!(out, vec![(OptionId::Hold, 1.0)]);
}

#[test]
fn arg_max_takes_the_last_of_equal_best_and_hold_when_empty() {
    // The same choice DO27's hysteresis loop makes on its first decision (`max_by(total_cmp)`).
    let mut core = AiCore::new(3, AiCadence::new(1, 1), ArgMax);
    let units = [1u32, 2, 3];
    core.decide(0, &units, |id, _rng, out| match *id {
        1 => out.extend([
            (OptionId::Push, 0.4),
            (OptionId::Kite, 0.7),
            (OptionId::Flank, 0.7),
        ]),
        2 => {}
        _ => out.extend([(OptionId::Retreat, 0.1)]),
    });
    assert_eq!(core.decision_for(1), Some(OptionId::Flank));
    assert_eq!(core.decision_for(2), Some(OptionId::Hold));
    assert_eq!(core.decision_for(3), Some(OptionId::Retreat));
    let rows = core.rows(1);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].entity, 1);
    assert_eq!(rows[0].current, Some(OptionId::Flank));
    assert_eq!(rows[0].next_decision_tick, 1);
    core.forget(2);
    assert_eq!(core.decision_for(2), None);
}

#[test]
fn rows_name_each_units_phase_and_next_decision() {
    let mut core = AiCore::new(3, AiCadence::default(), ArgMax);
    let units = [4u32, 17];
    for tick in 0..10u64 {
        core.decide(tick, &units, |_, _, out| out.push((OptionId::Defend, 0.9)));
    }
    let rows = core.rows(10);
    assert_eq!(
        rows.iter()
            .map(|r| (r.entity, r.decision_phase, r.next_decision_tick))
            .collect::<Vec<_>>(),
        [(4, 4, 14), (17, 7, 17)]
    );
    let text = serde_json::to_string(&rows).unwrap_or_default();
    assert!(text.contains("\"current\":\"Defend\""), "{text}");
}

#[test]
fn an_order_commits_outside_the_schedule_and_a_snapshot_resumes_bit_for_bit() {
    let catalogue = catalogue();
    let mut core = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    // A squad order commits at once on a tick the unit is not due on; its next due tick re-scores it.
    *core.decision_entry(5) = Some(OptionId::Regroup);
    let world = make_world(0);
    run_tick(&mut core, 0, &world, &catalogue);
    assert_eq!(
        core.decision_for(5),
        Some(OptionId::Regroup),
        "unit 5 is not due on tick 0"
    );
    let mut plain = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    run_tick(&mut plain, 0, &world, &catalogue);
    for tick in 1..=5u64 {
        run_tick(&mut core, tick, &make_world(tick), &catalogue);
        run_tick(&mut plain, tick, &make_world(tick), &catalogue);
    }
    assert!(core.decision_for(5).is_some());
    assert_eq!(
        core.decisions(),
        plain.decisions(),
        "after its due tick the order no longer shows"
    );

    // A snapshot taken mid-run (serde, as a save or a late-join state) resumes with the same decisions.
    let text = serde_json::to_string(&core).unwrap_or_default();
    let restored: Result<AiCore<ArgMax>, _> = serde_json::from_str(&text);
    let Ok(mut restored) = restored else {
        panic!("the AI core snapshot did not read back: {text}");
    };
    assert_eq!(restored.decisions(), core.decisions());
    for tick in 6..60u64 {
        let world = make_world(tick);
        run_tick(&mut core, tick, &world, &catalogue);
        run_tick(&mut restored, tick, &world, &catalogue);
        assert_eq!(restored.decisions(), core.decisions(), "tick {tick}");
    }
    assert_eq!(restored.world_seed(), core.world_seed());
    assert_eq!(restored.cadence(), core.cadence());
}

/// FNV-1a over little-endian words.
fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// The curve outputs over 1025 inputs per curve, as bits.
fn curve_bits() -> u64 {
    let curves = [
        ResponseCurve::linear(1.0),
        ResponseCurve::linear(0.7),
        ResponseCurve::quadratic(),
        ResponseCurve::inverse_quadratic(),
        ResponseCurve::logistic(12.0, 0.5),
        ResponseCurve::logistic(8.0, 0.3),
        ResponseCurve::inverse_logistic(12.0, 0.3),
        ResponseCurve::inverse_logistic(8.0, 0.5),
        ResponseCurve::plateau(0.6),
        ResponseCurve::plateau(0.0),
        ResponseCurve::band(0.5, 0.2),
        ResponseCurve::band(0.25, 0.0),
    ];
    let kinds: Vec<CurveKind> = curves.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&CurveKind::Band) && kinds.contains(&CurveKind::InverseLogistic));
    fnv(curves
        .iter()
        .flat_map(|c| (0..=1024u32).map(move |i| u64::from(c.evaluate(i as f32 / 1024.0).to_bits()))))
}

/// The generator's words, floats and ranges over several seeds and streams.
fn rng_bits() -> u64 {
    let mut words = Vec::new();
    for seed in [0u64, 1, 0xD027_A1C0, u64::MAX] {
        let mut r = AiRng::new(seed);
        for _ in 0..256 {
            words.push(r.next_u64());
            words.push(u64::from(r.next_f32().to_bits()));
            words.push(r.range(-5, 12) as u64);
        }
        for id in 0..64u32 {
            words.push(AiRng::mix(seed, u64::from(id)));
            words.push(AiRng::unit_stream(seed, id).next_u64());
            words.push(AiRng::tick_stream(seed, u64::from(id) * 7, id).next_u64());
        }
    }
    fnv(words)
}

/// The committed decisions of the 120-tick test world.
fn decision_bits() -> u64 {
    let catalogue = catalogue();
    let mut core = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    let mut words = Vec::new();
    for tick in 0..120u64 {
        let world = make_world(tick);
        run_tick(&mut core, tick, &world, &catalogue);
        words.extend(
            core.decisions()
                .iter()
                .map(|(id, d)| (u64::from(*id) << 32) | d.map_or(99, |o| u64::from(o.bit()))),
        );
    }
    fnv(words)
}

#[test]
fn ai_bits_match_the_pinned_hashes_on_every_arch() {
    // The pins are the same on x86-64 and aarch64 (the cross-arch proof runs this test there too: the Mac,
    // the Android test device and qemu-user). A change here is a sim-version change (ADR-8012 A-4).
    let got = [curve_bits(), rng_bits(), decision_bits()];
    eprintln!(
        "ai bits: curves {:#018x} rng {:#018x} decisions {:#018x}",
        got[0], got[1], got[2]
    );
    assert_eq!(got, PINNED, "AI core bits moved: curves, rng, decisions");
}

/// Pinned on x86-64 Linux (the build server) and matched on aarch64 macOS (debug and release); see `ai_bits_match_the_pinned_hashes_on_every_arch`.
const PINNED: [u64; 3] = [
    0x2b06_9b35_e8c5_1014,
    0xa72b_3b58_bfd9_9a39,
    0x03ed_3246_30eb_379d,
];

// ---------------------------------------------------------------------------
// Team rosters and the id index (plan row S-09 re-copy, regia-absorb-4)
// ---------------------------------------------------------------------------
//
// DO27's SIM_TICK_PERF fix keeps every result bit-identical: the per-team lists are built once per tick in
// world order, and an id -> slot index answers what a forward `find` answered. These tests hold the general
// engine types to the same contract: a roster equals the linear filter it replaces, slot for slot, and the AI
// decisions (and the pins above) do not move when the scorer reads rosters instead of scanning the world.

use regia_detsim::ai::{RosterError, RosterSettings, SlotIndex, TeamRosters, team_group_mask};
use regia_detsim::hash::{Mix, canonical_f32_bits};

/// One row of the moving test world: entity id, team, XZ position, hit points (0: dead, off every roster).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Body {
    id: u32,
    team: u8,
    pos: [f32; 2],
    hp: i32,
}

impl Body {
    fn alive(&self) -> bool {
        self.hp > 0
    }
}

/// The team a live body is listed under; dead bodies are on no roster.
fn roster_team(body: &Body) -> Option<u8> {
    body.alive().then_some(body.team)
}

/// A seeded world of `n` bodies in `teams` teams: bodies walk toward their nearest enemy, take damage from
/// enemies within 20 m, die, are removed from the middle of the array every 8 ticks, and respawn at the end
/// with fresh ids. Every query reads either the linear scan (`Lookup::Scan`) or the rosters
/// (`Lookup::Rosters`); both must give the same bits.
struct Scene {
    rows: Vec<Body>,
    teams: u8,
    next_id: u32,
    rng: AiRng,
    rosters: TeamRosters,
    index: SlotIndex,
    others: Vec<u32>,
    /// Calls of the team test (`roster_team`) the queries made this run: the scan cost.
    team_reads: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Scan,
    Rosters,
}

impl Scene {
    fn new(n: u32, teams: u8, seed: u64) -> Self {
        let mut rng = AiRng::new(seed);
        let side = math::sqrt(n as f32 * 400.0);
        let rows = (0..n)
            .map(|k| Body {
                id: 10_000 + k * 3,
                team: (rng.next_u64() % u64::from(teams)) as u8,
                pos: [rng.next_f32() * side, rng.next_f32() * side],
                hp: 40 + (rng.next_u64() % 60) as i32,
            })
            .collect();
        Scene {
            rows,
            teams,
            next_id: 10_000 + n * 3,
            rng,
            rosters: TeamRosters::new(RosterSettings::default()),
            index: SlotIndex::default(),
            others: Vec::new(),
            team_reads: 0,
        }
    }

    /// The nearest live enemy of the body in `slot`: `(distance, enemy slot)`; `f32::MAX` when none.
    fn nearest_enemy(&mut self, slot: usize, lookup: Lookup) -> (f32, usize) {
        let me = self.rows[slot];
        let mut best = (f32::MAX, usize::MAX);
        let mut consider = |other: usize, rows: &[Body]| {
            let o = rows[other];
            let dx = o.pos[0] - me.pos[0];
            let dz = o.pos[1] - me.pos[1];
            let d = math::sqrt(dx * dx + dz * dz);
            if d < best.0 {
                best = (d, other);
            }
        };
        match lookup {
            Lookup::Scan => {
                for other in 0..self.rows.len() {
                    self.team_reads += 1;
                    if roster_team(&self.rows[other]).is_some_and(|t| t != me.team) {
                        consider(other, &self.rows);
                    }
                }
            }
            Lookup::Rosters => {
                self.rosters.others_into(me.team, &mut self.others);
                for &other in &self.others {
                    consider(other as usize, &self.rows);
                }
            }
        }
        best
    }

    /// Live enemies of the body in `slot` within 20 m, in ascending slot order.
    fn enemies_near(&mut self, slot: usize, lookup: Lookup) -> u32 {
        let me = self.rows[slot];
        let near = |o: &Body| {
            let dx = o.pos[0] - me.pos[0];
            let dz = o.pos[1] - me.pos[1];
            dx * dx + dz * dz <= 400.0
        };
        match lookup {
            Lookup::Scan => {
                let mut n = 0;
                for other in 0..self.rows.len() {
                    self.team_reads += 1;
                    if roster_team(&self.rows[other]).is_some_and(|t| t != me.team) && near(&self.rows[other])
                    {
                        n += 1;
                    }
                }
                n
            }
            Lookup::Rosters => {
                self.rosters.others_into(me.team, &mut self.others);
                self.others
                    .iter()
                    .filter(|&&o| near(&self.rows[o as usize]))
                    .count() as u32
            }
        }
    }

    /// One tick: rosters and the index are rebuilt once (the roster path), every live body walks 1 m toward
    /// its nearest enemy and takes one point per enemy within 20 m; every 8th tick the dead are removed and
    /// as many fresh bodies join at the end.
    fn step(&mut self, tick: u64, lookup: Lookup) {
        if lookup == Lookup::Rosters {
            let reads = &mut self.team_reads;
            self.rosters
                .build(&self.rows, |b| {
                    *reads += 1;
                    roster_team(b)
                })
                .expect("teams fit the default settings");
            self.index.build(&self.rows, |b| b.id);
        }
        let mut moves = Vec::with_capacity(self.rows.len());
        for slot in 0..self.rows.len() {
            if !self.rows[slot].alive() {
                moves.push(([0.0, 0.0], 0));
                continue;
            }
            let (d, enemy) = self.nearest_enemy(slot, lookup);
            let hits = self.enemies_near(slot, lookup);
            let step = if enemy == usize::MAX || d <= 1e-3 {
                [0.0, 0.0]
            } else {
                let e = self.rows[enemy].pos;
                let me = self.rows[slot].pos;
                [(e[0] - me[0]) / d, (e[1] - me[1]) / d]
            };
            moves.push((step, hits as i32));
        }
        for (body, (step, hits)) in self.rows.iter_mut().zip(moves) {
            body.pos[0] += step[0];
            body.pos[1] += step[1];
            body.hp -= hits;
        }
        if tick % 8 == 7 {
            let dead = self.rows.iter().filter(|b| !b.alive()).count();
            self.rows.retain(Body::alive);
            for _ in 0..dead {
                let team = (self.rng.next_u64() % u64::from(self.teams)) as u8;
                let pos = [self.rng.next_f32() * 200.0, self.rng.next_f32() * 200.0];
                self.rows.push(Body {
                    id: self.next_id,
                    team,
                    pos,
                    hp: 60,
                });
                self.next_id += 1;
            }
        }
    }

    /// The tick's state word: every row's id, team, canonical position bits and hit points, in array order.
    fn state_word(&self) -> u64 {
        let mut mix = Mix::new(0x5101_7e57);
        for b in &self.rows {
            mix.u64(u64::from(b.id) << 8 | u64::from(b.team));
            mix.u64(u64::from(canonical_f32_bits(b.pos[0])) << 32 | u64::from(canonical_f32_bits(b.pos[1])));
            mix.u64(b.hp as u64);
        }
        mix.finish()
    }
}

/// SIM_TICK_PERF's chain: FNV-1a over every tick's state word.
fn scene_chain(n: u32, ticks: u64, lookup: Lookup) -> (u64, u64) {
    let mut scene = Scene::new(n, 4, 0x51A7_E000 ^ u64::from(n));
    let mut words = Vec::with_capacity(ticks as usize);
    for tick in 0..ticks {
        scene.step(tick, lookup);
        words.push(scene.state_word());
    }
    (fnv(words), scene.team_reads)
}

#[test]
fn rosters_match_a_linear_scan_every_tick() {
    let mut scene = Scene::new(900, 4, 0xD1E5_0900);
    let mut rebuilt = SlotIndex::default();
    let mut deaths = 0usize;
    for tick in 0..48u64 {
        // The roster path rebuilds both structures at the top of the tick.
        scene.step(tick, Lookup::Rosters);
        scene.rosters.build(&scene.rows, roster_team).expect("four teams");
        let rows = &scene.rows;
        deaths += rows.iter().filter(|b| !b.alive()).count();
        for team in 0..scene.teams {
            let scan: Vec<u32> = (0..rows.len() as u32)
                .filter(|&s| roster_team(&rows[s as usize]) == Some(team))
                .collect();
            assert_eq!(
                scene.rosters.team(team),
                scan.as_slice(),
                "tick {tick} team {team}"
            );
            let enemies: Vec<u32> = (0..rows.len() as u32)
                .filter(|&s| roster_team(&rows[s as usize]).is_some_and(|t| t != team))
                .collect();
            let mut others = Vec::new();
            scene.rosters.others_into(team, &mut others);
            assert_eq!(others, enemies, "tick {tick} enemies of team {team}");
        }
        assert!(scene.rosters.team(9).is_empty() && scene.rosters.team(255).is_empty());
        let live = rows.iter().filter(|b| b.alive()).count();
        assert_eq!(scene.rosters.len(), live, "tick {tick}");
        for (slot, b) in rows.iter().enumerate() {
            assert_eq!(
                scene.rosters.team_of_slot(slot),
                roster_team(b),
                "tick {tick} slot {slot}"
            );
        }
        // The id index answers exactly what a forward scan answers, for present and absent ids.
        rebuilt.build(rows, |b| b.id);
        for probe in rows
            .iter()
            .map(|b| b.id)
            .chain([0, 9_999, u32::MAX, scene.next_id])
        {
            let want = rows.iter().position(|b| b.id == probe);
            assert_eq!(rebuilt.slot_of(probe), want, "tick {tick} id {probe}");
            assert_eq!(
                rebuilt.find(rows, probe, |b| b.id),
                want,
                "tick {tick} id {probe}"
            );
        }
        assert_eq!(rebuilt.len(), rows.len());
    }
    assert!(deaths > 0, "bodies must die in the run");
    assert!(scene.next_id > 10_000 + 900 * 3, "dead bodies must be replaced");
}

#[test]
fn rosters_keep_the_tick_chain_bit_for_bit() {
    // DO27 pinned SIM_TICK_PERF's per-tick hash chains before optimizing and kept them; here the same chain
    // is folded with the linear scan and with the rosters (300 bodies x 240 ticks, 900 x 90) and must agree,
    // then match the pin.
    let (scan_300, _) = scene_chain(300, 240, Lookup::Scan);
    let (roster_300, _) = scene_chain(300, 240, Lookup::Rosters);
    let (scan_900, _) = scene_chain(900, 90, Lookup::Scan);
    let (roster_900, _) = scene_chain(900, 90, Lookup::Rosters);
    eprintln!("ROSTER_CHAIN 300x240 {roster_300:#018x} 900x90 {roster_900:#018x}");
    assert_eq!(roster_300, scan_300, "rosters changed the 300-body chain");
    assert_eq!(roster_900, scan_900, "rosters changed the 900-body chain");
    assert_eq!([roster_300, roster_900], ROSTER_CHAINS, "the scene's chain moved");
}

/// Pinned on x86-64 Linux (the build server, debug); see `rosters_keep_the_tick_chain_bit_for_bit`.
const ROSTER_CHAINS: [u64; 2] = [0xaea3_5e9b_e883_d72d, 0x1bab_5434_d295_0498];

#[test]
fn rosters_read_each_row_once_per_tick_instead_of_once_per_unit() {
    // The cost of the per-unit filter is `units x rows` team reads per query; the roster build reads each
    // row once per tick and the queries read rosters only. Counters, no timing.
    let n = 900u32;
    let ticks = 4u64;
    let (_, scan_reads) = scene_chain(n, ticks, Lookup::Scan);
    let (_, roster_reads) = scene_chain(n, ticks, Lookup::Rosters);
    assert_eq!(
        roster_reads,
        u64::from(n) * ticks,
        "one roster build per tick, one read per row"
    );
    // Two queries per live body per tick, each a full scan before any body dies.
    assert!(scan_reads >= 2 * u64::from(n) * u64::from(n), "{scan_reads}");
    let mut rosters = TeamRosters::new(RosterSettings::default());
    let rows: Vec<Body> = Scene::new(n, 4, 7).rows;
    for _ in 0..ticks {
        rosters.build(&rows, roster_team).expect("four teams");
    }
    let stats = rosters.stats();
    assert_eq!(stats.builds, ticks);
    assert_eq!(stats.rows_read, u64::from(n) * ticks);
    assert_eq!(stats.members, rows.iter().filter(|b| b.alive()).count());
    assert_eq!(stats.teams, 4);
}

#[test]
fn roster_scoring_keeps_the_pinned_decisions() {
    // The pinned decision run (`decision_bits`) with the nearest-enemy input read from rosters.
    let catalogue = catalogue();
    let mut core = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    let mut rosters = TeamRosters::new(RosterSettings::default());
    let mut enemies = Vec::new();
    let mut words = Vec::new();
    for tick in 0..120u64 {
        let world = make_world(tick);
        rosters.build(&world, |u| Some(u.team)).expect("two teams");
        core.decide(tick, &world, |unit, rng, out| {
            rosters.others_into(unit.team, &mut enemies);
            let mut nearest = f32::MAX;
            for &slot in &enemies {
                let other = world[slot as usize];
                let dx = other.pos[0] - unit.pos[0];
                let dz = other.pos[1] - unit.pos[1];
                let d = math::sqrt(dx * dx + dz * dz);
                if d < nearest {
                    nearest = d;
                }
            }
            let inputs = Inputs {
                hull_pct: rng.next_f32(),
                ammo_pct: rng.next_f32(),
                distance_to_threat_frac: det_clamp(nearest / 300.0, 0.0, 1.0),
                range_band_frac: det_clamp(nearest / 150.0, 0.0, 1.0),
                local_force_ratio: 0.5,
                ..Inputs::default()
            };
            score_options(&catalogue, &inputs, |_| true, |_| 1.0, out);
        });
        words.extend(
            core.decisions()
                .iter()
                .map(|(id, d)| (u64::from(*id) << 32) | d.map_or(99, |o| u64::from(o.bit()))),
        );
    }
    assert_eq!(fnv(words), PINNED[2], "rosters moved the pinned decisions");
}

#[test]
fn id_index_matches_a_scan_after_a_middle_remove() {
    // DO27's id-index test: 40 ids, a middle remove shifts the tail, a dropped index still answers through the
    // scan fallback, and an id that comes back at the end is found again.
    let mut rows: Vec<u32> = (0..40u32).map(|n| 5_000 + n).collect();
    let mut index = SlotIndex::default();
    index.build(&rows, |&id| id);
    let check = |index: &SlotIndex, rows: &[u32]| {
        for (slot, &id) in rows.iter().enumerate() {
            assert_eq!(index.find(rows, id, |&id| id), Some(slot), "id {id}");
            assert_eq!(index.slot_of(id), Some(slot), "id {id}");
        }
        assert_eq!(index.find(rows, 9_999_999, |&id| id), None);
    };
    check(&index, &rows);
    let middle = rows[20];
    let shifted = rows[21];
    rows.remove(20);
    index.remove(20);
    check(&index, &rows);
    assert_eq!(index.find(&rows, shifted, |&id| id), Some(20));
    // A remove the index never heard of: the stale slot is caught and the forward scan answers.
    let before = index.stats();
    rows.remove(0);
    assert_eq!(index.find(&rows, shifted, |&id| id), Some(19));
    let after = index.stats();
    assert_eq!(after.scan_fallbacks, before.scan_fallbacks + 1);
    index.build(&rows, |&id| id);
    // A dropped index still answers through the scan.
    index.clear();
    assert_eq!(index.find(&rows, shifted, |&id| id), Some(19));
    index.build(&rows, |&id| id);
    rows.push(middle);
    index.push(middle);
    check(&index, &rows);
}

#[test]
fn id_index_keeps_the_first_of_duplicate_ids() {
    // A forward scan returns the first row of an id; so does the index, through removes and pushes.
    let mut rows = vec![7u32, 3, 7, 9, 3, 7];
    let mut index = SlotIndex::default();
    index.build(&rows, |&id| id);
    let scan = |rows: &[u32], id: u32| rows.iter().position(|&r| r == id);
    for id in [3, 7, 9, 1] {
        assert_eq!(index.slot_of(id), scan(&rows, id));
    }
    for remove in [0usize, 1, 0] {
        rows.remove(remove);
        index.remove(remove);
        for id in [3, 7, 9, 1] {
            assert_eq!(
                index.slot_of(id),
                scan(&rows, id),
                "after removing slot {remove}: {rows:?}"
            );
            assert_eq!(index.find(&rows, id, |&r| r), scan(&rows, id));
        }
    }
    rows.push(3);
    index.push(3);
    rows.push(1);
    index.push(1);
    for id in [3, 7, 9, 1] {
        assert_eq!(index.slot_of(id), scan(&rows, id), "{rows:?}");
    }
    assert_eq!(
        index.stats().scan_fallbacks,
        0,
        "reported changes never need the scan"
    );
}

#[test]
fn a_team_past_the_setting_is_refused_with_a_stable_code() {
    let mut rosters = TeamRosters::new(RosterSettings { max_teams: 4 });
    let rows = [1u8, 2, 3, 4, 0];
    let err = rosters
        .build(&rows, |&t| Some(t))
        .expect_err("team 4 is past max_teams 4");
    assert_eq!(
        err,
        RosterError::TeamOutOfRange {
            slot: 3,
            team: 4,
            max_teams: 4
        }
    );
    assert_eq!(err.code(), "roster_team_out_of_range");
    assert!(rosters.is_empty(), "a refused build leaves empty rosters");
    rosters.build(&rows[..3], |&t| Some(t)).expect("teams 1-3 fit");
    assert_eq!(rosters.team(2), &[1]);
    assert_eq!(rosters.team_count(), 4);
    assert_eq!(team_group_mask(0), 1);
    assert_eq!(team_group_mask(5), 1 << 5);
    assert_eq!(team_group_mask(64), 0, "teams past 63 have no spatial group bit");
}

#[test]
fn the_state_word_folds_negative_zero_to_positive_zero() {
    // DO27's 300-body chain moved once when its hash learned the canonical float rule (-0 folds to +0); the
    // scene chain above uses the same rule, so a body parked at -0.0 hashes like one at +0.0.
    let mut a = Scene::new(4, 2, 1);
    let mut b = Scene::new(4, 2, 1);
    a.rows[0].pos = [-0.0, 0.0];
    b.rows[0].pos = [0.0, 0.0];
    assert_eq!(a.state_word(), b.state_word());
    a.rows[0].pos = [f32::from_bits(0x7fc0_0001), 0.0];
    b.rows[0].pos = [f32::NAN, 0.0];
    assert_eq!(a.state_word(), b.state_word());
}

// ---------------------------------------------------------------------------
// Think cadence channels (audit gap AI-3): the default settings keep DO27's bits
// ---------------------------------------------------------------------------

#[test]
fn default_cadence_is_bit_identical() {
    use regia_detsim::ai::{CadenceChannel, CadenceTier, ChannelId, PhaseRule};
    // The default cadence is two channels, `decide` (10 ticks) and `sight` (6), on the id-modulo phase,
    // uncapped, with no tiers: DO27's `AiSchedule`.
    let default = AiCadence::default();
    assert_eq!(
        default.channels(),
        [
            CadenceChannel::new("decide", AiSchedule::DECISION_TICKS),
            CadenceChannel::new("sight", AiSchedule::SIGHT_TICKS),
        ]
    );
    assert_eq!(default.channels()[0].phase, PhaseRule::IdModulo);
    assert_eq!(default.channels()[0].max_per_tick, None);
    assert!(default.tiers().is_empty());
    // The same schedule spelled out (an explicit 1x tier and an extra channel nobody runs) decides the same.
    let explicit = AiCadence::new(10, 6)
        .with_tiers(vec![CadenceTier::new(1)])
        .and_then(|c| c.with_channel(CadenceChannel::new("route", 30)))
        .expect("valid settings");
    for cadence in [default.clone(), explicit] {
        let catalogue = catalogue();
        let mut core = AiCore::new(0xD027_A1C0, cadence.clone(), ArgMax);
        let mut words = Vec::new();
        let mut out = Vec::new();
        let mut probe = AiCore::new(0xD027_A1C0, cadence.clone(), ArgMax);
        for tick in 0..120u64 {
            let world = make_world(tick);
            // Every channel's due list is the legacy predicate, in entity-id order.
            probe
                .due_units(ChannelId::DECIDE, tick, &world, &mut out)
                .expect("decide is always there");
            let listed: Vec<u32> = out.iter().map(|&i| world[i].id).collect();
            let legacy: Vec<u32> = world
                .iter()
                .filter(|u| AiSchedule::due_to_decide(tick, u.id))
                .map(|u| u.id)
                .collect();
            assert_eq!(listed, legacy, "tick {tick}");
            probe
                .due_units(ChannelId::SIGHT, tick, &world, &mut out)
                .expect("sight is always there");
            let listed: Vec<u32> = out.iter().map(|&i| world[i].id).collect();
            let legacy: Vec<u32> = world
                .iter()
                .filter(|u| u64::from(AiSchedule::sight_phase(u.id)) == tick % 6)
                .map(|u| u.id)
                .collect();
            assert_eq!(listed, legacy, "tick {tick}");
            for id in 0..64u32 {
                assert_eq!(
                    cadence.due(ChannelId::DECIDE, tick, id),
                    AiSchedule::due_to_decide(tick, id)
                );
                assert_eq!(
                    cadence.next_tick(ChannelId::DECIDE, tick, id),
                    Some(cadence.next_decision_tick(tick, id))
                );
            }
            run_tick(&mut core, tick, &world, &catalogue);
            words.extend(
                core.decisions()
                    .iter()
                    .map(|(id, d)| (u64::from(*id) << 32) | d.map_or(99, |o| u64::from(o.bit()))),
            );
        }
        assert_eq!(
            fnv(words),
            PINNED[2],
            "decision bits moved under the default cadence"
        );
    }
    assert_eq!([curve_bits(), rng_bits(), decision_bits()], PINNED);
}

// ---------------------------------------------------------------------------
// The catalogue as data (gap AI-7): the pinned decisions through the index path
// ---------------------------------------------------------------------------

#[test]
fn pinned_decisions_hold_through_the_catalogue_index_path() {
    // `decision_bits` with the test catalogue as a `UtilityCatalogue` (slice order, preset inputs), scored by
    // index and committed by `ArgMaxIdx`: the same words, so the same pin.
    use regia_detsim::ai::{ArgMaxIdx, OptionIdx, UtilityCatalogue, score_options_idx};
    let enum_catalogue = catalogue();
    let cat = UtilityCatalogue::from_enum_options("ai_core_test", &enum_catalogue).expect("test catalogue");
    let bit_of = |idx: OptionIdx| u64::from(enum_catalogue[idx.index()].id.bit());
    let mut core = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMaxIdx);
    let mut words = Vec::new();
    for tick in 0..120u64 {
        let world = make_world(tick);
        core.decide(tick, &world, |unit, rng, out| {
            let inputs = inputs_for(unit, &world, rng);
            score_options_idx(&cat, &inputs.values(), |_| true, |_| 1.0, out);
        });
        words.extend(
            core.decisions()
                .iter()
                .map(|(id, d)| (u64::from(*id) << 32) | d.map_or(99, bit_of)),
        );
    }
    assert_eq!(fnv(words), PINNED[2], "the index path moved the pinned decisions");
}
