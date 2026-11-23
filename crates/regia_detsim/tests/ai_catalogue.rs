// SPDX-License-Identifier: MPL-2.0
//! A game's own utility catalogue as data (audit gap AI-7): `UtilityCatalogue`, the `.detutility.ron` format and
//! DO27's catalogue as its preset with the same bits.

use regia_detsim::ai::{
    AiCadence, AiCore, AiLevel, AiRng, ArgMax, ArgMaxIdx, DetUtilityError, DifficultyProfile, Inputs,
    OptionId, OptionIdx, UtilityCatalogue, catalogue, migrate_det_utility_ron, score, score_options_idx,
};
use regia_detsim::float::det_clamp;
use regia_detsim::math;

/// A seeded input in -0.25..1.25 (so the curves' clamps are exercised), with every 97th draw an edge value.
fn draw(rng: &mut AiRng, case: u32, field: u32) -> f32 {
    if (case + field).is_multiple_of(97) {
        const EDGES: [f32; 6] = [0.0, -0.0, 1.0, 0.5, f32::NAN, f32::INFINITY];
        return EDGES[((case / 97 + field) % EDGES.len() as u32) as usize];
    }
    rng.next_f32() * 1.5 - 0.25
}

fn seeded_inputs(rng: &mut AiRng, case: u32) -> Inputs {
    Inputs {
        hull_pct: draw(rng, case, 0),
        ammo_pct: draw(rng, case, 1),
        range_band_frac: draw(rng, case, 2),
        local_force_ratio: draw(rng, case, 3),
        cover_availability: draw(rng, case, 4),
        distance_to_leader_frac: draw(rng, case, 5),
        distance_to_threat_frac: draw(rng, case, 6),
        threat_incoming: draw(rng, case, 7),
        commander_priority: draw(rng, case, 8),
        scrap_proximity: draw(rng, case, 9),
        point_distance_frac: draw(rng, case, 10),
        leader_target_focus: draw(rng, case, 11),
        relative_energy_frac: draw(rng, case, 12),
        turn_rate_frac: draw(rng, case, 13),
        hopper_frac: draw(rng, case, 14),
    }
}

#[test]
fn preset_scores_equal_the_enum_scores_bit_for_bit() {
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    let mut compared = 0u32;
    for level in [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard] {
        let profile = DifficultyProfile::for_level(level);
        let enum_catalogue = catalogue(&profile.thresholds);
        let preset = UtilityCatalogue::do27_preset(&profile.thresholds);
        assert_eq!(preset.options().len(), 12);
        let mut rng = AiRng::new(0xA17_CA7A_1060 ^ level as u64);
        let (mut by_enum, mut by_idx) = (Vec::new(), Vec::new());
        for case in 0..10_000u32 {
            let inputs = seeded_inputs(&mut rng, case);
            let values = inputs.values();
            for option in &enum_catalogue {
                let a = score(option, &inputs);
                let b = preset.score(option.id.idx(), &values).expect("preset option");
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "{level:?} case {case} option {:?}: enum {a} catalogue {b}",
                    option.id
                );
                fold = (fold ^ u64::from(b.to_bits())).wrapping_mul(0x0000_0100_0000_01b3);
                compared += 1;
            }
            // The level's filter and bias over the preset equal the enum scorer's, key for key.
            by_enum.clear();
            by_idx.clear();
            profile.score_options(&enum_catalogue, &inputs, &mut by_enum);
            profile.score_options_idx(&preset, &values, &mut by_idx);
            assert_eq!(by_enum.len(), by_idx.len());
            for ((id, a), (idx, b)) in by_enum.iter().zip(&by_idx) {
                assert_eq!(id.idx(), *idx);
                assert_eq!(a.to_bits(), b.to_bits(), "{level:?} case {case} filtered {id:?}");
            }
        }
    }
    assert_eq!(compared, 3 * 10_000 * 12);
    eprintln!("PRESET_SCORE_FOLD {fold:#018x} over {compared} scores");
}

#[test]
fn the_index_core_commits_the_enum_cores_decisions() {
    let profile = DifficultyProfile::hard();
    let enum_catalogue = catalogue(&profile.thresholds);
    let preset = UtilityCatalogue::do27_preset(&profile.thresholds);
    let mut by_enum = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMax);
    let mut by_idx = AiCore::new(0xD027_A1C0, AiCadence::default(), ArgMaxIdx);
    let units: Vec<u32> = (1..=40).collect();
    for tick in 0..200u64 {
        let mut case = 0u32;
        by_enum.decide(tick, &units, |_, rng, out| {
            let inputs = seeded_inputs(rng, case);
            case += 1;
            profile.score_options(&enum_catalogue, &inputs, out);
        });
        let mut case = 0u32;
        by_idx.decide(tick, &units, |_, rng, out| {
            let inputs = seeded_inputs(rng, case);
            case += 1;
            profile.score_options_idx(&preset, &inputs.values(), out);
        });
    }
    assert_eq!(by_enum.decisions().len(), 40);
    for (id, decision) in by_enum.decisions() {
        assert_eq!(
            by_idx.decision_for(*id),
            decision.map(OptionId::idx),
            "unit {id} decided differently by index"
        );
    }
}

/// A game's catalogue: a scavenger that eats, flees or fights. `ammo` is the new input, `forage` the new option.
const GAME: &str = r#"(
    format_version: 1,
    name: "scavenger",
    inputs: [
        (name: "hunger"),
        (name: "threat"),
        (name: "food_near"),
        (name: "ammo"),
    ],
    options: [
        (
            name: "forage",
            considerations: [
                (input: "hunger", curve: (kind: Linear, m: 1.0)),
                (input: "food_near", curve: (kind: Plateau, k: 0.5), weight: 0.5),
                (input: "threat", curve: (kind: InverseQuadratic), multiplier: true),
            ],
        ),
        (
            name: "flee",
            considerations: [
                (input: "threat", curve: (kind: Quadratic)),
                (input: "ammo", curve: (kind: Linear, m: -1.0), weight: 0.25),
            ],
        ),
        (
            name: "fight",
            min_score: 0.1,
            considerations: [
                (input: "threat", curve: (kind: Band, b: 0.6, c: 0.2)),
                (input: "ammo", curve: (kind: Linear, m: 1.0), multiplier: true),
            ],
        ),
    ],
)
"#;

/// The scavenger's decision written out by hand (no catalogue code): the scores, then the last of the best.
fn reference(hunger: f32, threat: f32, food_near: f32, ammo: f32) -> (&'static str, [f32; 3]) {
    let c = |x: f32| det_clamp(x, 0.0, 1.0);
    let (hunger, threat, food_near, ammo) = (c(hunger), c(threat), c(food_near), c(ammo));
    // forage: (hunger * 1 + plateau(food_near) * 0.5) / 1.5, times 1 - (1 - threat)^2.
    let plateau = if food_near >= 0.5 { 1.0 } else { food_near / 0.5 };
    let inv_quad = 1.0 - (1.0 - threat) * (1.0 - threat);
    let forage = c((hunger + 0.5 * plateau) / 1.5 * c(inv_quad));
    // flee: (threat^2 * 1 + clamp(-ammo) * 0.25) / 1.25; the negative slope clamps to 0.
    let flee = c((threat * threat + 0.25 * c(-ammo)) / 1.25);
    // fight: the band around 0.6 (half-width 0.2), times ammo.
    let d = threat - 0.6;
    let fight = c(c(math::exp(-(d * d) / (2.0 * 0.2 * 0.2))) * ammo);
    let scores = [forage, flee, fight];
    let names = ["forage", "flee", "fight"];
    let mut best = 0;
    for (i, s) in scores.iter().enumerate().skip(1) {
        if *s >= scores[best] {
            best = i;
        }
    }
    (names[best], scores)
}

#[test]
fn a_game_adds_an_option_and_an_input() {
    let cat = UtilityCatalogue::from_ron_str(GAME).expect("the scavenger catalogue loads");
    assert_eq!(cat.options().len(), 3);
    assert_eq!(cat.inputs().len(), 4);
    assert_eq!(cat.option_by_name("fight"), Some(OptionIdx(2)));
    assert_eq!(cat.input_by_name("ammo").map(|i| i.0), Some(3));
    assert_eq!(cat.option(OptionIdx(2)).map(|o| o.min_score), Some(0.1));
    let mut scores = Vec::new();
    let mut decided = [0u32; 3];
    let steps = 24;
    for h in 0..=steps {
        for t in 0..=steps {
            for f in [0.0, 0.3, 0.8] {
                for a in [0.0, 0.4, 1.0] {
                    let (hunger, threat) = (h as f32 / steps as f32, t as f32 / steps as f32);
                    let values = [hunger, threat, f, a];
                    scores.clear();
                    score_options_idx(&cat, &values, |_| true, |_| 1.0, &mut scores);
                    let (want, want_scores) = reference(hunger, threat, f, a);
                    for ((_, got), want) in scores.iter().zip(want_scores) {
                        assert!(
                            (got - want).abs() < 1e-6,
                            "{values:?}: score {got} vs reference {want}"
                        );
                    }
                    let mut policy_decision = None;
                    regia_detsim::ai::DecisionPolicy::commit(
                        &ArgMaxIdx,
                        &mut policy_decision,
                        scores.as_slice(),
                    );
                    let got = policy_decision.and_then(|i| cat.option_name(i));
                    assert_eq!(got, Some(want), "{values:?}: scores {scores:?}");
                    decided[cat.option_by_name(want).map_or(0, |i| i.index())] += 1;
                }
            }
        }
    }
    // Each option wins somewhere on the grid, so the reference is exercised on all three.
    assert!(
        decided.iter().all(|n| *n > 0),
        "decisions per option: {decided:?}"
    );

    // The AI core runs it: units commit catalogue options by index.
    let mut core = AiCore::new(7, AiCadence::default(), ArgMaxIdx);
    let units: Vec<u32> = (1..=10).collect();
    for tick in 0..10u64 {
        core.decide(tick, &units, |id, _, out| {
            let threat = *id as f32 / 10.0;
            score_options_idx(&cat, &[0.2, threat, 0.0, 1.0], |_| true, |_| 1.0, out);
        });
    }
    for id in &units {
        let threat = *id as f32 / 10.0;
        let (want, _) = reference(0.2, threat, 0.0, 1.0);
        assert_eq!(
            core.decision_for(*id).and_then(|i| cat.option_name(i)),
            Some(want),
            "unit {id}"
        );
    }

    // The explain rows carry the catalogue's names.
    let explain = cat
        .explain(OptionIdx(0), &[1.0, 0.5, 1.0, 0.0])
        .expect("forage explains");
    assert_eq!(explain.option, "forage");
    let names: Vec<&str> = explain.considerations.iter().map(|c| c.input.as_str()).collect();
    assert_eq!(names, ["hunger", "food_near", "threat"]);
}

fn refused(text: &str) -> DetUtilityError {
    match UtilityCatalogue::from_ron_str(text) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(e) => e,
    }
}

#[test]
fn bad_catalogues_are_refused_with_named_errors() {
    let unknown = GAME.replace(
        "(input: \"ammo\", curve: (kind: Linear, m: 1.0), multiplier: true)",
        "(input: \"armor\", curve: (kind: Linear, m: 1.0))",
    );
    assert_eq!(
        refused(&unknown),
        DetUtilityError::UnknownInput {
            option: "fight".into(),
            input: "armor".into()
        }
    );
    assert_eq!(refused(&unknown).code(), "det_utility_unknown_input");

    let dup_option = GAME.replace("name: \"flee\"", "name: \"forage\"");
    assert_eq!(
        refused(&dup_option),
        DetUtilityError::DuplicateName {
            kind: "option",
            name: "forage".into()
        }
    );
    let dup_input = GAME.replace("(name: \"ammo\")", "(name: \"threat\")");
    let e = refused(&dup_input);
    assert_eq!(
        e,
        DetUtilityError::DuplicateName {
            kind: "input",
            name: "threat".into()
        }
    );
    assert_eq!(e.code(), "det_utility_duplicate_name");

    let nan_curve = GAME.replace("(kind: Plateau, k: 0.5)", "(kind: Plateau, k: NaN)");
    let e = refused(&nan_curve);
    assert_eq!(
        e,
        DetUtilityError::NonFinite {
            option: "forage".into(),
            field: "considerations[1].curve.k".into()
        }
    );
    assert_eq!(e.code(), "det_utility_non_finite");
    let inf_weight = GAME.replace("weight: 0.25", "weight: inf");
    assert_eq!(refused(&inf_weight).code(), "det_utility_non_finite");
    let nan_floor = GAME.replace("min_score: 0.1", "min_score: NaN");
    assert_eq!(
        refused(&nan_floor),
        DetUtilityError::NonFinite {
            option: "fight".into(),
            field: "min_score".into()
        }
    );

    let many: String = (0..65)
        .map(|i| format!("(name: \"o{i}\", considerations: [])"))
        .collect::<Vec<_>>()
        .join(", ");
    let over = format!("(format_version: 1, inputs: [], options: [{many}])");
    let e = refused(&over);
    assert_eq!(
        e,
        DetUtilityError::OverCap {
            what: "options".into(),
            count: 65,
            max: 64
        }
    );
    assert_eq!(e.code(), "det_utility_over_cap");
    // The cap itself loads.
    let at_cap: String = (0..64)
        .map(|i| format!("(name: \"o{i}\", considerations: [])"))
        .collect::<Vec<_>>()
        .join(", ");
    let cap =
        UtilityCatalogue::from_ron_str(&format!("(format_version: 1, inputs: [], options: [{at_cap}])"))
            .expect("64 options load");
    assert!(cap.all_options().contains(OptionIdx(63)));

    let empty_name = GAME.replace("name: \"flee\"", "name: \"\"");
    assert_eq!(refused(&empty_name).code(), "det_utility_bad_name");
    let typo = GAME.replace(
        "multiplier: true),\n            ],\n        ),\n        (\n            name: \"flee\"",
        "multiplyer: true),\n            ],\n        ),\n        (\n            name: \"flee\"",
    );
    assert_eq!(refused(&typo).code(), "det_utility_parse");
    assert_eq!(refused("not ron at all").code(), "det_utility_parse");
}

#[test]
fn catalogue_ron_round_trips() {
    let cat = UtilityCatalogue::from_ron_str(GAME).expect("loads");
    let text = cat.to_ron();
    let again = UtilityCatalogue::from_ron_str(&text).expect("canonical text loads");
    assert_eq!(again, cat);
    assert_eq!(again.to_ron(), text, "the canonical text is a fixed point");

    // The DO27 preset survives the file form bit for bit, at every level.
    for level in [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard] {
        let t = DifficultyProfile::for_level(level).thresholds;
        let preset = UtilityCatalogue::do27_preset(&t);
        let loaded = UtilityCatalogue::from_ron_str(&preset.to_ron()).expect("preset text loads");
        assert_eq!(loaded, preset, "{level:?}");
        let mut rng = AiRng::new(99);
        for case in 0..500 {
            let values = seeded_inputs(&mut rng, case).values();
            for i in 0..12u16 {
                let (a, b) = (
                    preset.score(OptionIdx(i), &values),
                    loaded.score(OptionIdx(i), &values),
                );
                assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits));
            }
        }
    }
}

#[test]
fn newer_version_is_refused() {
    // A newer file is refused as newer, even with fields this build does not know.
    let v2 = GAME.replace("format_version: 1,", "format_version: 2,\n    tie_break: Random,");
    let e = refused(&v2);
    assert_eq!(
        e,
        DetUtilityError::NewerVersion {
            found: 2,
            supported: 1
        }
    );
    assert_eq!(e.code(), "det_utility_newer_version");
    assert_eq!(migrate_det_utility_ron(&v2), Err(e));
    let v0 = GAME.replace("format_version: 1,", "format_version: 0,");
    assert_eq!(refused(&v0).code(), "det_utility_unsupported_version");
    // A file without a version is v0, never published.
    let unversioned = GAME.replace("format_version: 1,", "");
    assert_eq!(refused(&unversioned).code(), "det_utility_unsupported_version");
    // Version 1 needs no migration.
    assert_eq!(migrate_det_utility_ron(GAME).as_deref(), Ok(GAME));
}

#[test]
fn newer_version_with_retyped_fields_is_refused_as_newer() {
    // Review nit (lane v06-det-utility-fixes): a v2 body whose known fields changed type, not just
    // an extra field, is still refused by its version before the typed parse reads a field.
    let v2 = r#"(
    format_version: 2,
    name: 7,
    inputs: { "hunger": 0, "threat": 1 },
    options: "see options.ron",
)"#;
    let e = refused(v2);
    assert_eq!(
        e,
        DetUtilityError::NewerVersion {
            found: 2,
            supported: 1
        }
    );
    assert_eq!(e.code(), "det_utility_newer_version");
    // The same body at v1 is a typed-parse error, not a version error.
    let v1 = v2.replace("format_version: 2,", "format_version: 1,");
    assert_ne!(refused(&v1).code(), "det_utility_newer_version");
}

#[test]
fn an_index_core_snapshot_resumes_bit_for_bit() {
    // `AiCore<ArgMaxIdx, OptionIdx>` keeps its decisions (by index) in a snapshot, like `AiCore<ArgMax>`.
    let cat = UtilityCatalogue::from_ron_str(GAME).expect("loads");
    let units: Vec<u32> = (1..=30).collect();
    let step = |core: &mut AiCore<ArgMaxIdx, OptionIdx>, tick: u64| {
        core.decide(tick, &units, |id, rng, out| {
            let values = [rng.next_f32(), rng.next_f32(), *id as f32 / 30.0, rng.next_f32()];
            score_options_idx(&cat, &values, |_| true, |_| 1.0, out);
        });
    };
    let mut straight = AiCore::new(0x5EED, AiCadence::default(), ArgMaxIdx);
    let mut resumed = AiCore::new(0x5EED, AiCadence::default(), ArgMaxIdx);
    for tick in 0..40 {
        step(&mut straight, tick);
        step(&mut resumed, tick);
    }
    let text = serde_json::to_string(&resumed).expect("snapshot");
    let mut resumed: AiCore<ArgMaxIdx, OptionIdx> = serde_json::from_str(&text).expect("restore");
    for tick in 40..120 {
        step(&mut straight, tick);
        step(&mut resumed, tick);
    }
    assert_eq!(straight.decisions(), resumed.decisions());
    assert_eq!(straight.decisions().len(), 30);
    let rows = straight.rows(120);
    assert!(
        rows.iter()
            .all(|r| r.current.is_some_and(|i| cat.option_name(i).is_some()))
    );
}
