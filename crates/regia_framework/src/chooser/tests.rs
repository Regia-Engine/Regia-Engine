// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 8 tests 7-14, 15 (trace contents), 16 and 19 (choosers). The allocation half of test
//! 15 is `tests/chooser_alloc.rs` (it installs a counting allocator).

use std::collections::BTreeMap;

use regia_core::math::Rng;

use super::*;
use crate::tags::{MAX_QUERY_DEPTH, MAX_QUERY_NODES};

/// 1,000 picks of `WEIGHTED` (weights 1, 2, 1) with `Rng::new(7)`, computed independently from
/// the PCG32 reference (O'Neill 2014) in a separate script, not by this code.
const GOLDEN_WEIGHTED: &str = concat!(
    "bbbbacccaabbbabbbaaabbcbabacbcbbaaabcbcbbacababbbbcccbbbccbbbcabbbcbabaaaabbbcbcbcbcaacbbbababbcbbac",
    "ababacbcabcbccaaabbcccbbabbbbaabcbbccabcbbcabbbccbbabbababaabbbbcbabbbbcbbbabbcacbaabbbacabaabbabcab",
    "accbbbcbaaaacaacbcbbbbaabbcaaacbacabbcccccacbabcacbababcabbbbabcbacaccbababacbbababaabababcacbcccaba",
    "baacbababcbabbbbabbacbcbbbabaacccaacbaabbcbccbbbbacccbcbcabcaabbbbacbacabcbbababbbcbcababbaaabaacccb",
    "baacabaabcccbabbacbabbbaabaaccbbbbccacbabccaccabbacbccbbccbcbbbbbbaabcbcbbbbbcacbbaaacababacbccababc",
    "abbbcabcbbccabbbbcbabbbbcbbcaccbcabbacabbcbabbbbbbabcbbbaabccbcbbbcabbcabaccbcbbcaabbabaaabbbbbacbcc",
    "bbaababcbbcccbcbbabbaaabbbabbabbcbabbcccbbaabbcccbccccacccbcacbaaabbabaaccacababcbbcacbbbaabcabbbcbb",
    "babaababbcabccbbbacbacbbabbbbbabbaabcacaaabbbcbbbacbbbacacaabbcbabbbabaaacbaccccbbbccababaccbbcabbcc",
    "bbbbbabbaaaaaccbbccaabbbcbabbcbccbbcbabacbbcaababcbcbcacbbbcccacacccbbbbbccacacacbbbcbbbaaabbbcbbacb",
    "cbbcbbcacbcbcbbaaababbcbbbababbcbbbcabccccbcbbacababaccbcbabbccbbacabcbccccbbccabbabbbbcccacabbabbbb",
);

fn table(text: &str) -> ChooserTable {
    ChooserTable::from_ron_str(text).unwrap_or_else(|e| panic!("table did not load: {e}"))
}

fn ctx(values: &[(&str, ChooserValue)]) -> ChooserContext {
    let mut ctx = ChooserContext::default();
    for (name, value) in values {
        ctx.set(name, value.clone());
    }
    ctx
}

fn pick(table: &ChooserTable, ctx: &mut ChooserContext) -> Option<String> {
    table
        .evaluate(&NoChoosers, ctx, &mut Rng::new(0), None)
        .and_then(|o| o.row)
}

fn codes(issues: &[ChooserIssue]) -> Vec<&'static str> {
    issues.iter().map(|i| i.code).collect()
}

fn load_err(text: &str) -> Vec<ChooserIssue> {
    ChooserTable::from_ron_str(text)
        .expect_err("must not load")
        .issues
}

const SPEC_EXAMPLE: &str = r#"(
    format_version: 1,
    mode: Best,
    result: Asset("montage"),
    inputs: { "hit_angle": Float, "armed": Bool, "stance": Enum(["stand", "crouch"]) },
    outputs: { "rate": Float },
    columns: [
        (id: "armed",  input: "armed",     kind: Bool),
        (id: "stance", input: "stance",    kind: Enum),
        (id: "angle",  input: "hit_angle", kind: FloatDistance(influence: 1.0, max_distance: 180.0, wrap: Some((-180.0, 180.0)))),
        (id: "rate",   output: "rate",     kind: SetFloat),
    ],
    rows: [
        (id: "hit_front", cells: { "armed": True, "angle": 0.0, "rate": 1.0 }, result: Asset("anim/hit_front.montage.ron")),
        (id: "hit_back",  cells: { "angle": 180.0, "stance": Is("crouch"), "rate": 1.2 }, result: Asset("anim/hit_back.montage.ron")),
        (id: "nested",    enabled: false, cells: {}, result: Chooser("anim/hit_generic.chooser.ron")),
    ],
    catch_all: Some((result: Asset("anim/hit_generic.montage.ron"), outputs: { "rate": 1.0 })),
)"#;

#[test]
fn the_spec_example_loads_and_picks_by_angle() {
    let t = table(SPEC_EXAMPLE);
    let mut c = ctx(&[
        ("armed", ChooserValue::Bool(true)),
        ("stance", ChooserValue::Enum("crouch".into())),
        ("hit_angle", ChooserValue::Float(170.0)),
    ]);
    let outcome = t
        .evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None)
        .expect("pick");
    assert_eq!(outcome.row.as_deref(), Some("hit_back"));
    assert_eq!(c.output("rate"), Some(&ChooserValue::Float(1.2)));
    // -170 is 10 degrees from 180 across the seam, so the back hit still wins.
    c.set("hit_angle", ChooserValue::Float(-170.0));
    assert_eq!(pick(&t, &mut c).as_deref(), Some("hit_back"));
    // Standing: the back row's stance gate fails; unarmed: the front row fails; catch-all.
    c.set("stance", ChooserValue::Enum("stand".into()));
    c.set("armed", ChooserValue::Bool(false));
    let outcome = t
        .evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None)
        .expect("catch-all");
    assert!(outcome.used_catch_all);
    assert_eq!(
        outcome.result,
        ResultEntry::Asset("anim/hit_generic.montage.ron".into())
    );
}

// Test 7: one test per gate kind, with absent inputs and NaN.

fn gate_table(kind: &str, input_kind: &str, cells: &[(&str, &str)], options: &str) -> ChooserTable {
    let rows: Vec<String> = cells
        .iter()
        .map(|(id, cell)| {
            format!(r#"(id: "{id}", cells: {{ "g": {cell} }}, result: Asset("x/{id}.clip.ron"))"#)
        })
        .collect();
    table(&format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "v": {input_kind} }},
            columns: [(id: "g", input: "v", kind: {kind}{options})],
            rows: [{}])"#,
        rows.join(", ")
    ))
}

#[test]
fn bool_gates() {
    let t = gate_table("Bool", "Bool", &[("yes", "True"), ("no", "False")], "");
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Bool(false))])).as_deref(),
        Some("no")
    );
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Bool(true))])).as_deref(),
        Some("yes")
    );
    assert_eq!(
        pick(&t, &mut ChooserContext::default()),
        None,
        "absent input fails every non-Any gate"
    );
    let any = gate_table("Bool", "Bool", &[("any", "Any")], "");
    assert_eq!(pick(&any, &mut ChooserContext::default()).as_deref(), Some("any"));
}

#[test]
fn int_and_float_ranges_including_the_wrap_seam() {
    let t = gate_table("IntRange", "Int", &[("low", "(0, 9)"), ("high", "(10, 20)")], "");
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Int(9))])).as_deref(),
        Some("low")
    );
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Int(20))])).as_deref(),
        Some("high")
    );
    assert_eq!(pick(&t, &mut ctx(&[("v", ChooserValue::Int(21))])), None);
    // Kinds must match: an Int input never satisfies a FloatRange column (and vice versa).
    let f = gate_table("FloatRange", "Float", &[("mid", "(0.5, 1.0)")], "");
    assert_eq!(
        pick(&f, &mut ctx(&[("v", ChooserValue::Float(0.5))])).as_deref(),
        Some("mid")
    );
    assert_eq!(pick(&f, &mut ctx(&[("v", ChooserValue::Float(0.4))])), None);
    assert_eq!(pick(&f, &mut ctx(&[("v", ChooserValue::Int(1))])), None);
    assert_eq!(
        pick(&f, &mut ctx(&[("v", ChooserValue::Float(f32::NAN))])),
        None,
        "NaN fails"
    );
    // A heading range running past 360 back to 0: [350, 10] on a (0, 360) circle.
    let w = gate_table(
        "FloatRange",
        "Float",
        &[("north", "(350.0, 10.0)")],
        ", wrap: Some((0.0, 360.0))",
    );
    for (angle, hit) in [
        (355.0, true),
        (5.0, true),
        (0.0, true),
        (360.0, true),
        (725.0, true),
        (20.0, false),
        (340.0, false),
    ] {
        assert_eq!(
            pick(&w, &mut ctx(&[("v", ChooserValue::Float(angle))])).is_some(),
            hit,
            "angle {angle}"
        );
    }
    let wi = gate_table(
        "IntRange",
        "Int",
        &[("night", "(22, 4)")],
        ", wrap: Some((0.0, 24.0))",
    );
    assert!(pick(&wi, &mut ctx(&[("v", ChooserValue::Int(23))])).is_some());
    assert!(pick(&wi, &mut ctx(&[("v", ChooserValue::Int(3))])).is_some());
    assert!(pick(&wi, &mut ctx(&[("v", ChooserValue::Int(12))])).is_none());
}

#[test]
fn enum_is_is_not_and_one_of() {
    let rows = [("crouch", r#"Is("crouch")"#), ("not_prone", r#"IsNot("prone")"#)];
    let t = gate_table("Enum", r#"Enum(["stand", "crouch", "prone"])"#, &rows, "");
    let e = |v: &str| ctx(&[("v", ChooserValue::Enum(v.into()))]);
    assert_eq!(pick(&t, &mut e("crouch")).as_deref(), Some("crouch"));
    assert_eq!(pick(&t, &mut e("stand")).as_deref(), Some("not_prone"));
    assert_eq!(pick(&t, &mut e("prone")), None);
    let one_of = gate_table(
        "Enum",
        r#"Enum(["stand", "crouch", "prone"])"#,
        &[("low", r#"OneOf(["crouch", "prone"])"#)],
        "",
    );
    assert!(pick(&one_of, &mut e("prone")).is_some());
    assert!(pick(&one_of, &mut e("stand")).is_none());
}

#[test]
fn name_case_option() {
    let exact = gate_table("Name", "Name", &[("boss", r#""Boss""#)], "");
    let n = |v: &str| ctx(&[("v", ChooserValue::Name(v.into()))]);
    assert!(pick(&exact, &mut n("Boss")).is_some());
    assert!(pick(&exact, &mut n("boss")).is_none());
    let loose = gate_table(
        "Name",
        "Name",
        &[("boss", r#""Boss""#)],
        ", case_sensitive: false",
    );
    assert!(pick(&loose, &mut n("BOSS")).is_some());
}

#[test]
fn tags_gate_with_a_query() {
    let t = gate_table(
        "Tags",
        "Tags",
        &[(
            "burning",
            r#"AllOf([Has("Status.Burning"), NoneOf([Has("Status.Wet")])])"#,
        )],
        "",
    );
    let tags = |names: &[&str]| {
        ctx(&[(
            "v",
            ChooserValue::Tags(crate::tags::TagContainer::from_names(names.iter().copied()).expect("tags")),
        )])
    };
    assert!(
        pick(&t, &mut tags(&["Status.Burning.Hot"])).is_some(),
        "a child tag matches `Has`"
    );
    assert!(pick(&t, &mut tags(&["Status.Burning", "Status.Wet"])).is_none());
    assert!(pick(&t, &mut tags(&[])).is_none());
}

#[test]
fn asset_is_and_kind() {
    let t = gate_table(
        "Asset",
        "Asset",
        &[
            ("exact", r#"Is("fx/boom.cue.ron")"#),
            ("any_montage", r#"Kind("montage")"#),
        ],
        "",
    );
    let a = |v: &str| ctx(&[("v", ChooserValue::Asset(v.into()))]);
    assert_eq!(pick(&t, &mut a("fx/boom.cue.ron")).as_deref(), Some("exact"));
    assert_eq!(
        pick(&t, &mut a("anim/x.montage.ron")).as_deref(),
        Some("any_montage")
    );
    assert_eq!(pick(&t, &mut a("anim/x.clip.ron")), None);
    assert_eq!(asset_kind("fx/boom.wav"), "wav");
    assert_eq!(asset_kind("plain.ron"), "ron");
    assert_eq!(asset_kind("noext"), "");
}

// Test 8: modes, disabled rows and columns.

const MODES: &str = r#"(format_version: 1, mode: MODE, result: Asset("clip"),
    inputs: { "speed": Float },
    columns: [
        (id: "fast", input: "speed", kind: FloatRange),
        (id: "near", input: "speed", kind: FloatDistance(max_distance: 10.0)),
    ],
    rows: [
        (id: "r0", cells: { "fast": (5.0, 100.0), "near": 6.0 }, result: Asset("a/r0.clip.ron")),
        (id: "r1", cells: { "near": 4.0 }, result: Asset("a/r1.clip.ron")),
        (id: "r2", cells: { "near": 4.0 }, result: Asset("a/r2.clip.ron")),
        (id: "r3", enabled: false, cells: { "near": 5.0 }, result: Asset("a/r3.clip.ron")),
        (id: "r4", cells: { "fast": (50.0, 100.0) }, result: Asset("a/r4.clip.ron")),
    ])"#;

#[test]
fn first_returns_the_earliest_candidate_and_marks_later_rows_not_evaluated() {
    let t = table(&MODES.replace("MODE", "First"));
    let mut c = ctx(&[("speed", ChooserValue::Float(5.0))]);
    let (outcome, trace) = t.evaluate_traced(&NoChoosers, &mut c, &mut Rng::new(0), None, TraceDetail::Brief);
    assert_eq!(outcome.and_then(|o| o.row).as_deref(), Some("r0"));
    let outcomes: Vec<&RowOutcome> = trace.rows.iter().map(|r| &r.outcome).collect();
    assert_eq!(
        outcomes,
        [
            &RowOutcome::Selected,
            &RowOutcome::NotEvaluated,
            &RowOutcome::NotEvaluated,
            &RowOutcome::NotEvaluated,
            &RowOutcome::NotEvaluated
        ]
    );
}

#[test]
fn best_picks_the_lowest_cost_and_breaks_ties_to_the_earlier_row() {
    let t = table(&MODES.replace("MODE", "Best"));
    // speed 4: r0 rejected (fast), r1 and r2 cost 0 (tie, r1 wins), r3 disabled, r4 rejected.
    assert_eq!(
        pick(&t, &mut ctx(&[("speed", ChooserValue::Float(4.0))])).as_deref(),
        Some("r1")
    );
    // speed 6: r0 cost 0 beats r1/r2 cost 0.2.
    assert_eq!(
        pick(&t, &mut ctx(&[("speed", ChooserValue::Float(6.0))])).as_deref(),
        Some("r0")
    );
    // A disabled row never wins, even with the lowest cost (r3 targets 5.0).
    assert_eq!(
        pick(&t, &mut ctx(&[("speed", ChooserValue::Float(5.0))])).as_deref(),
        Some("r0")
    );
}

#[test]
fn all_returns_every_candidate_in_row_order() {
    let t = table(&MODES.replace("MODE", "All"));
    let mut c = ctx(&[("speed", ChooserValue::Float(60.0))]);
    let rows: Vec<Option<String>> = t
        .evaluate_all(&NoChoosers, &mut c)
        .into_iter()
        .map(|o| o.row)
        .collect();
    assert_eq!(
        rows,
        [
            Some("r0".into()),
            Some("r1".into()),
            Some("r2".into()),
            Some("r4".into())
        ]
    );
}

#[test]
fn a_disabled_column_counts_as_any() {
    let mut t = table(&MODES.replace("MODE", "First"));
    t.columns[0].disabled = true;
    // speed 1 would fail r0's `fast` gate; with the column disabled r0 passes.
    assert_eq!(
        pick(&t, &mut ctx(&[("speed", ChooserValue::Float(1.0))])).as_deref(),
        Some("r0")
    );
}

// Test 9: scoring.

fn cost_of(kind: &str, target: f32, input: f32) -> Option<f32> {
    let t = table(&format!(
        r#"(format_version: 1, mode: Best, result: Asset("clip"), inputs: {{ "v": Float }},
            columns: [(id: "d", input: "v", kind: {kind})],
            rows: [(id: "r", cells: {{ "d": {target:?} }}, result: Asset("a/r.clip.ron"))])"#
    ));
    let (_, trace) = t.evaluate_traced(
        &NoChoosers,
        &mut ctx(&[("v", ChooserValue::Float(input))]),
        &mut Rng::new(0),
        None,
        TraceDetail::Full,
    );
    match trace.rows[0].outcome {
        RowOutcome::Selected => match trace.rows[0].cells[0].verdict {
            Verdict::Score(cost) => Some(cost),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn distance_cost_wraps_clamps_scales_and_drops() {
    let angle = "FloatDistance(influence: 1.0, max_distance: 180.0, wrap: Some((-180.0, 180.0)))";
    // 170 and -170 are 20 apart across the seam.
    assert_eq!(cost_of(angle, 170.0, -170.0), Some(20.0 / 180.0));
    assert_eq!(cost_of(angle, -180.0, 180.0), Some(0.0));
    // Clamp: a gap past max_distance costs the full influence.
    assert_eq!(cost_of("FloatDistance(max_distance: 10.0)", 0.0, 50.0), Some(1.0));
    // Influence scales.
    assert_eq!(
        cost_of("FloatDistance(influence: 4.0, max_distance: 10.0)", 0.0, 5.0),
        Some(2.0)
    );
    // drop_beyond filters rows beyond max_distance.
    assert_eq!(
        cost_of("FloatDistance(max_distance: 10.0, drop_beyond: true)", 0.0, 10.5),
        None
    );
    assert_eq!(
        cost_of("FloatDistance(max_distance: 10.0, drop_beyond: true)", 0.0, 10.0),
        Some(1.0)
    );
}

/// A wrapped gap that is not finite (an infinite input, or a difference that overflows `f32`)
/// costs the full influence like an absent input, and Best still picks the exact match (audit
/// framework5 #5: the cost never becomes NaN, because the clamp's `min` drops a NaN gap).
#[test]
fn a_non_finite_wrapped_gap_costs_the_full_influence_and_best_still_picks_the_match() {
    let wrapped = "FloatDistance(influence: 1.0, max_distance: 10.0, wrap: Some((0.0, 360.0)))";
    assert_eq!(cost_of(wrapped, 0.0, f32::INFINITY), Some(1.0));
    assert_eq!(cost_of(wrapped, -2e38, 2e38), Some(1.0));
    let t = table(&format!(
        r#"(format_version: 1, mode: Best, result: Asset("clip"), inputs: {{ "v": Float }},
            columns: [(id: "d", input: "v", kind: {wrapped})],
            rows: [
                (id: "far", cells: {{ "d": -2e38 }}, result: Asset("a/far.clip.ron")),
                (id: "match", cells: {{ "d": 2e38 }}, result: Asset("a/match.clip.ron")),
            ])"#
    ));
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Float(2e38))])).as_deref(),
        Some("match")
    );
}

/// With `drop_beyond`, a wrapped gap that is not finite is beyond `max_distance` and drops the
/// row, as an absent input or an unwrapped infinite gap does (audit framework5 #5).
#[test]
fn drop_beyond_drops_a_row_whose_wrapped_gap_is_not_finite() {
    let wrapped = "FloatDistance(max_distance: 10.0, wrap: Some((0.0, 360.0)), drop_beyond: true)";
    assert_eq!(cost_of(wrapped, 0.0, f32::INFINITY), None);
    assert_eq!(cost_of(wrapped, -2e38, 2e38), None);
    // Unwrapped, the same inputs were already beyond.
    let plain = "FloatDistance(max_distance: 10.0, drop_beyond: true)";
    assert_eq!(cost_of(plain, 0.0, f32::INFINITY), None);
    // A finite wrapped gap inside the limit still passes.
    assert_eq!(cost_of(wrapped, 355.0, 5.0), Some(1.0));
}

// Test 10: weighted.

const WEIGHTED: &str = r#"(format_version: 1, mode: Weighted, result: Asset("cue"),
    columns: [(id: "w", kind: Weight)],
    rows: [
        (id: "a", cells: { "w": 1.0 }, result: Asset("s/a.cue.ron")),
        (id: "b", cells: { "w": 2.0 }, result: Asset("s/b.cue.ron")),
        (id: "c", cells: { "w": 1.0 }, result: Asset("s/c.cue.ron")),
    ])"#;

#[test]
fn weighted_picks_follow_a_fixed_golden_list_and_the_weights() {
    let t = table(WEIGHTED);
    let mut rng = Rng::new(7);
    let mut c = ChooserContext::default();
    let picks: String = (0..1000)
        .map(|_| {
            let row = t
                .evaluate(&NoChoosers, &mut c, &mut rng, None)
                .and_then(|o| o.row);
            row.as_deref().and_then(|r| r.chars().next()).unwrap_or('?')
        })
        .collect();
    assert_eq!(
        picks, GOLDEN_WEIGHTED,
        "golden list computed independently from the PCG32 reference"
    );
    // Within 3 sigma of the weights (binomial sigma: sqrt(n p (1 - p))).
    for (row, p) in [('a', 0.25_f64), ('b', 0.5), ('c', 0.25)] {
        let n = picks.chars().filter(|&ch| ch == row).count() as f64;
        let sigma = (1000.0 * p * (1.0 - p)).sqrt();
        assert!((n - 1000.0 * p).abs() <= 3.0 * sigma, "{row}: {n}");
    }
}

#[test]
fn near_best_restricts_the_draw_to_the_best_band() {
    let t = table(
        r#"(format_version: 1, mode: Weighted, result: Asset("cue"), inputs: { "v": Float },
            columns: [(id: "d", input: "v", kind: FloatDistance(max_distance: 10.0)), (id: "w", kind: Weight)],
            rows: [
                (id: "far", cells: { "d": 9.0, "w": 100.0 }, result: Asset("s/far.cue.ron")),
                (id: "near", cells: { "d": 0.0, "w": 1.0 }, result: Asset("s/near.cue.ron")),
            ],
            near_best: Some(0.1))"#,
    );
    let mut rng = Rng::new(3);
    for _ in 0..50 {
        let row = t
            .evaluate(
                &NoChoosers,
                &mut ctx(&[("v", ChooserValue::Float(0.0))]),
                &mut rng,
                None,
            )
            .and_then(|o| o.row);
        assert_eq!(row.as_deref(), Some("near"));
    }
}

#[test]
fn recency_never_repeats_while_an_alternative_exists() {
    let mut t = table(WEIGHTED);
    t.recency = Some(Recency {
        window: 1,
        penalty: 0.0,
    });
    let mut rng = Rng::new(11);
    let mut mem = RecentPicks::default();
    let mut last: Option<String> = None;
    for _ in 0..200 {
        let row = t
            .evaluate(
                &NoChoosers,
                &mut ChooserContext::default(),
                &mut rng,
                Some(&mut mem),
            )
            .and_then(|o| o.row);
        assert_ne!(row, last, "repeated");
        last = row;
    }
    assert_eq!(mem.rows.len(), MAX_RECENCY_WINDOW as usize);
    // The only survivor repeats: the penalty is ignored when it zeroes every participant.
    t.rows.truncate(1);
    let mut mem = RecentPicks::default();
    for _ in 0..3 {
        let row = t
            .evaluate(
                &NoChoosers,
                &mut ChooserContext::default(),
                &mut rng,
                Some(&mut mem),
            )
            .and_then(|o| o.row);
        assert_eq!(row.as_deref(), Some("a"));
    }
}

#[test]
fn zero_total_weight_takes_no_draw_and_other_modes_never_touch_the_rng() {
    let mut t = table(WEIGHTED);
    for row in &mut t.rows {
        row.cells.insert("w".into(), Cell::Weight(0.0));
    }
    let mut rng = Rng::new(5);
    let before = rng.clone();
    let (outcome, trace) = t.evaluate_traced(
        &NoChoosers,
        &mut ChooserContext::default(),
        &mut rng,
        None,
        TraceDetail::Brief,
    );
    assert_eq!(outcome.and_then(|o| o.row).as_deref(), Some("a"));
    assert_eq!(trace.draw, None);
    assert_eq!(rng, before);
    t.mode = Mode::Best;
    let _ = t.evaluate(&NoChoosers, &mut ChooserContext::default(), &mut rng, None);
    assert_eq!(rng, before);
}

// Test 11: catch-all.

#[test]
fn the_catch_all_answers_only_when_no_candidate_remains() {
    let t = table(
        r#"(format_version: 1, result: Value(Name), inputs: { "lvl": Int }, outputs: { "tier": Int },
            columns: [(id: "lvl", input: "lvl", kind: IntRange), (id: "tier", output: "tier", kind: SetInt)],
            rows: [(id: "high", cells: { "lvl": (10, 99), "tier": 3 }, result: Value("loot_rare"))],
            catch_all: Some((result: Value("loot_common"), outputs: { "tier": 1 })))"#,
    );
    let mut c = ctx(&[("lvl", ChooserValue::Int(12))]);
    let o = t
        .evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None)
        .expect("row");
    assert!(!o.used_catch_all);
    assert_eq!(
        o.result,
        ResultEntry::Value(ChooserValue::Name("loot_rare".into()))
    );
    assert_eq!(c.output("tier"), Some(&ChooserValue::Int(3)));
    c.set("lvl", ChooserValue::Int(2));
    let o = t
        .evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None)
        .expect("catch-all");
    assert!(o.used_catch_all);
    assert_eq!(o.row, None);
    assert_eq!(c.output("tier"), Some(&ChooserValue::Int(1)));
    let mut no_catch = t.clone();
    no_catch.catch_all = None;
    assert_eq!(
        no_catch.evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None),
        None
    );
}

// Test 12: nesting.

fn nested_env() -> BTreeMap<String, ChooserTable> {
    let mut env = BTreeMap::new();
    env.insert(
        "n/empty.chooser.ron".to_owned(),
        table(
            r#"(format_version: 1, result: Asset("clip"), inputs: { "v": Bool },
            columns: [(id: "v", input: "v", kind: Bool)],
            rows: [(id: "never", cells: { "v": True }, result: Asset("a/x.clip.ron"))])"#,
        ),
    );
    env.insert(
        "n/draw.chooser.ron".to_owned(),
        table(
            r#"(format_version: 1, mode: Weighted, result: Asset("clip"),
            rows: [(id: "only", result: Asset("a/drawn.clip.ron"))])"#,
        ),
    );
    env
}

#[test]
fn nested_tables_run_only_when_selected_and_an_empty_one_strikes_its_row() {
    let env = nested_env();
    let t = table(
        r#"(format_version: 1, mode: Best, result: Asset("clip"), inputs: { "go": Bool },
            columns: [(id: "go", input: "go", kind: Bool)],
            rows: [
                (id: "empty", cells: {}, result: Chooser("n/empty.chooser.ron")),
                (id: "drawn", cells: { "go": True }, result: Chooser("n/draw.chooser.ron")),
                (id: "inline", cells: {}, result: Inline((result: Asset("clip"),
                    rows: [(id: "in", result: Asset("a/inline.clip.ron"))]))),
            ])"#,
    );
    // go = false: `empty` is chosen first (tie, earliest), returns None and is struck; `drawn`
    // fails its gate, so `inline` answers. The weighted nested table never ran: no draw taken.
    let mut rng = Rng::new(9);
    let before = rng.clone();
    let mut c = ctx(&[
        ("go", ChooserValue::Bool(false)),
        ("v", ChooserValue::Bool(false)),
    ]);
    let (o, trace) = t.evaluate_traced(&env, &mut c, &mut rng, None, TraceDetail::Brief);
    let o = o.expect("inline");
    assert_eq!(o.row.as_deref(), Some("inline"));
    assert_eq!(o.result, ResultEntry::Asset("a/inline.clip.ron".into()));
    assert_eq!(rng, before, "the weighted nested table was not evaluated");
    assert_eq!(trace.rows[0].outcome, RowOutcome::NestedEmpty);
    assert_eq!(trace.nested.len(), 2, "the empty nested table and the inline one");
    // go = true: `empty` struck again, then `drawn` (row order tie-break) runs its nested draw.
    c.set("go", ChooserValue::Bool(true));
    let o = t.evaluate(&env, &mut c, &mut rng, None).expect("drawn");
    assert_eq!(o.result, ResultEntry::Asset("a/drawn.clip.ron".into()));
    assert_ne!(rng, before, "the nested weighted table took its draw");
}

fn chain(depth: usize) -> BTreeMap<String, ChooserTable> {
    let mut env = BTreeMap::new();
    for level in 0..depth {
        let result = if level + 1 == depth {
            r#"Asset("a/leaf.clip.ron")"#.to_owned()
        } else {
            format!(r#"Chooser("c/{}.chooser.ron")"#, level + 1)
        };
        env.insert(
            format!("c/{level}.chooser.ron"),
            table(&format!(
                r#"(format_version: 1, result: Asset("clip"), rows: [(id: "r{level}", result: {result})],
                    catch_all: Some((result: Asset("a/fallback{level}.clip.ron"))))"#
            )),
        );
    }
    env
}

#[test]
fn eight_levels_resolve_and_nine_answer_with_the_catch_all() {
    let env = chain(9); // c/0 nests c/1 .. c/8: eight nested levels.
    let top = env.get("c/0.chooser.ron").expect("top").clone();
    let o = top
        .evaluate(&env, &mut ChooserContext::default(), &mut Rng::new(0), None)
        .expect("leaf");
    assert_eq!(o.result, ResultEntry::Asset("a/leaf.clip.ron".into()));
    let env = chain(10); // nine nested levels: too deep.
    let top = env.get("c/0.chooser.ron").expect("top").clone();
    let (o, trace) = top.evaluate_traced(
        &env,
        &mut ChooserContext::default(),
        &mut Rng::new(0),
        None,
        TraceDetail::Brief,
    );
    let o = o.expect("catch-all");
    assert!(o.used_catch_all);
    assert_eq!(o.result, ResultEntry::Asset("a/fallback0.clip.ron".into()));
    assert!(trace.issues.contains(&"chooser_depth_exceeded"));
    assert!(codes(&top.validate(&env)).contains(&"chooser_depth_exceeded"));
}

/// A depth overrun evaluates a shallow `Chooser` catch-all once. The same catch-all that
/// itself nests past the limit is `Nothing` from `evaluate` and an empty list from
/// `evaluate_all` (no partial rows).
#[test]
fn depth_overrun_evaluates_a_shallow_chooser_catch_all_once() {
    let mut env = chain(10);
    env.insert(
        "safe.chooser.ron".into(),
        table(
            r#"(format_version: 1, result: Asset("clip"),
                rows: [(id: "ok", result: Asset("a/safe.clip.ron"))])"#,
        ),
    );
    let mut top = env.get("c/0.chooser.ron").expect("top").clone();
    top.catch_all = Some(CatchAll {
        result: ResultEntry::Chooser("safe.chooser.ron".into()),
        outputs: BTreeMap::new(),
    });
    let o = top
        .evaluate(&env, &mut ChooserContext::default(), &mut Rng::new(0), None)
        .expect("catch-all");
    assert!(o.used_catch_all);
    assert_eq!(o.result, ResultEntry::Asset("a/safe.clip.ron".into()));
    let (traced, trace) = top.evaluate_traced(
        &env,
        &mut ChooserContext::default(),
        &mut Rng::new(0),
        None,
        TraceDetail::Brief,
    );
    assert_eq!(traced.expect("traced").result, o.result);
    assert!(trace.issues.contains(&"chooser_depth_exceeded"));
    let all = top.evaluate_all(&env, &mut ChooserContext::default());
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].used_catch_all);
    assert_eq!(all[0].result, ResultEntry::Asset("a/safe.clip.ron".into()));

    let deep = chain(10);
    top.catch_all = Some(CatchAll {
        result: ResultEntry::Chooser("c/0.chooser.ron".into()),
        outputs: BTreeMap::new(),
    });
    let o = top
        .evaluate(&deep, &mut ChooserContext::default(), &mut Rng::new(0), None)
        .expect("nothing");
    assert!(o.used_catch_all);
    assert_eq!(o.result, ResultEntry::Nothing);
    assert!(
        top.evaluate_all(&deep, &mut ChooserContext::default()).is_empty(),
        "a catch-all that itself exceeds depth contributes no rows"
    );
}

#[test]
fn validate_reports_a_cycle_and_an_unresolved_reference() {
    let mut env = BTreeMap::new();
    for (me, next) in [("a", "b"), ("b", "a")] {
        env.insert(
            format!("{me}.chooser.ron"),
            table(&format!(
                r#"(format_version: 1, result: Asset("clip"), rows: [(id: "go", result: Chooser("{next}.chooser.ron"))])"#
            )),
        );
    }
    let a = env.get("a.chooser.ron").expect("a").clone();
    let issues = a.validate_at(Some("a.chooser.ron"), &env);
    let cycle = issues.iter().find(|i| i.code == "chooser_cycle").expect("cycle");
    assert_eq!(cycle.row.as_deref(), Some("go"));
    let lonely = table(
        r#"(format_version: 1, result: Asset("clip"), rows: [(id: "go", result: Chooser("gone.chooser.ron"))])"#,
    );
    let issues = lonely.validate(&NoChoosers);
    assert!(codes(&issues).contains(&"chooser_ref_unresolved"));
}

// Test 13: outputs.

#[test]
fn outputs_come_from_the_winner_and_never_alter_inputs() {
    let t = table(SPEC_EXAMPLE);
    let mut c = ctx(&[
        ("armed", ChooserValue::Bool(true)),
        ("hit_angle", ChooserValue::Float(0.0)),
    ]);
    let inputs_before: Vec<(String, ChooserValue)> =
        c.inputs().map(|(k, v)| (k.to_owned(), v.clone())).collect();
    let o = t
        .evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None)
        .expect("front");
    assert_eq!(o.row.as_deref(), Some("hit_front"));
    assert_eq!(o.outputs, [("rate".to_owned(), ChooserValue::Float(1.0))]);
    assert_eq!(c.output("rate"), Some(&ChooserValue::Float(1.0)));
    let inputs_after: Vec<(String, ChooserValue)> =
        c.inputs().map(|(k, v)| (k.to_owned(), v.clone())).collect();
    assert_eq!(inputs_before, inputs_after);
    assert_eq!(c.input("rate"), None);
}

// Test 14: one fixture per validation code, asserting code, row and column.

fn fixture(columns: &str, rows: &str, extra: &str) -> String {
    format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "b": Bool, "f": Float, "e": Enum(["x", "y"]) }},
            outputs: {{ "o": Float }}, columns: [{columns}], rows: [{rows}], catch_all: Some((result: Asset("a/c.clip.ron"))){extra})"#
    )
}

/// ADR-1917: free text went out through Rust's `{:?}`, so a NUL in a note became `\\0`, which RON refuses, and the
/// saved table no longer loaded.
#[test]
fn a_note_with_a_control_character_survives_the_writer() {
    let text = fixture(
        "",
        r#"(id: "r", cells: {}, result: Asset("a/c.clip.ron"), note: "a\u{0}b\"c")"#,
        "",
    );
    let table = ChooserTable::from_ron_str(&text).expect("loads");
    let written = table.to_ron_string();
    let again = ChooserTable::from_ron_str(&written).expect("the written table loads");
    assert_eq!(again.to_ron_string(), written);
    assert!(written.contains("note: "), "{written}");
}

fn first_issue(issues: &[ChooserIssue], code: &str) -> ChooserIssue {
    issues
        .iter()
        .find(|i| i.code == code)
        .cloned()
        .unwrap_or_else(|| panic!("no {code} in {issues:?}"))
}

#[test]
fn every_validation_code_has_a_fixture() {
    let row = r#"(id: "r", result: Asset("a/r.clip.ron"))"#;
    // chooser_too_many_rows
    let many: Vec<String> = (0..=MAX_ROWS)
        .map(|i| format!(r#"(id: "r{i}", result: Asset("a/r.clip.ron"))"#))
        .collect();
    assert!(codes(&load_err(&fixture("", &many.join(","), ""))).contains(&"chooser_too_many_rows"));
    // chooser_too_many_columns
    let cols: Vec<String> = (0..=MAX_COLUMNS)
        .map(|i| format!(r#"(id: "c{i}", input: "b", kind: Bool)"#))
        .collect();
    assert!(codes(&load_err(&fixture(&cols.join(","), row, ""))).contains(&"chooser_too_many_columns"));
    // chooser_too_many_params
    let params: Vec<String> = (0..=MAX_PARAMS).map(|i| format!(r#""p{i}": Bool"#)).collect();
    let text = format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ {} }}, rows: [{row}])"#,
        params.join(",")
    );
    assert!(codes(&load_err(&text)).contains(&"chooser_too_many_params"));
    // chooser_duplicate_row / chooser_duplicate_column
    let dup = first_issue(
        &load_err(&fixture("", &format!("{row},{row}"), "")),
        "chooser_duplicate_row",
    );
    assert_eq!(dup.row.as_deref(), Some("r"));
    let dup = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "b", kind: Bool), (id: "c", input: "b", kind: Bool)"#,
            row,
            "",
        )),
        "chooser_duplicate_column",
    );
    assert_eq!(dup.column.as_deref(), Some("c"));
    // chooser_unknown_input / chooser_kind_mismatch
    let unknown = first_issue(
        &load_err(&fixture(r#"(id: "c", input: "nope", kind: Bool)"#, row, "")),
        "chooser_unknown_input",
    );
    assert_eq!(unknown.column.as_deref(), Some("c"));
    let mismatch = first_issue(
        &load_err(&fixture(r#"(id: "c", input: "f", kind: Bool)"#, row, "")),
        "chooser_kind_mismatch",
    );
    assert_eq!(mismatch.column.as_deref(), Some("c"));
    // chooser_bad_cell
    let bad = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "b", kind: Bool)"#,
            r#"(id: "r", cells: { "c": 3.0 }, result: Asset("a/r.clip.ron"))"#,
            "",
        )),
        "chooser_bad_cell",
    );
    assert_eq!(
        (bad.row.as_deref(), bad.column.as_deref()),
        (Some("r"), Some("c"))
    );
    let undeclared = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "e", kind: Enum)"#,
            r#"(id: "r", cells: { "c": Is("z") }, result: Asset("a/r.clip.ron"))"#,
            "",
        )),
        "chooser_bad_cell",
    );
    assert_eq!(undeclared.row.as_deref(), Some("r"));
    // chooser_bad_range
    let range = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "f", kind: FloatRange)"#,
            r#"(id: "r", cells: { "c": (2.0, 1.0) }, result: Asset("a/r.clip.ron"))"#,
            "",
        )),
        "chooser_bad_range",
    );
    assert_eq!(
        (range.row.as_deref(), range.column.as_deref()),
        (Some("r"), Some("c"))
    );
    // chooser_bad_number: max_distance <= 0, negative weight, NaN.
    let number = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "f", kind: FloatDistance(max_distance: 0.0))"#,
            row,
            "",
        )),
        "chooser_bad_number",
    );
    assert_eq!(number.column.as_deref(), Some("c"));
    let weight = first_issue(
        &load_err(&fixture(
            r#"(id: "w", kind: Weight)"#,
            r#"(id: "r", cells: { "w": -1.0 }, result: Asset("a/r.clip.ron"))"#,
            "",
        )),
        "chooser_bad_number",
    );
    assert_eq!(
        (weight.row.as_deref(), weight.column.as_deref()),
        (Some("r"), Some("w"))
    );
    let nan = first_issue(
        &load_err(&fixture(
            r#"(id: "c", input: "f", kind: FloatDistance(max_distance: 1.0))"#,
            r#"(id: "r", cells: { "c": NaN }, result: Asset("a/r.clip.ron"))"#,
            "",
        )),
        "chooser_bad_number",
    );
    assert_eq!(nan.row.as_deref(), Some("r"));
    // chooser_ref_unresolved, chooser_cycle, chooser_depth_exceeded: see the nesting tests.
    // chooser_result_type_mismatch
    let mismatch = first_issue(
        &load_err(&fixture("", r#"(id: "r", result: Asset("a/r.cue.ron"))"#, "")),
        "chooser_result_type_mismatch",
    );
    assert_eq!(mismatch.row.as_deref(), Some("r"));
    // chooser_unreachable_row (warning) and chooser_no_catch_all (info): the table still loads.
    let t = table(
        r#"(format_version: 1, result: Asset("clip"), inputs: { "b": Bool },
            columns: [(id: "c", input: "b", kind: Bool)],
            rows: [(id: "first", cells: { "c": True }, result: Asset("a/1.clip.ron")),
                   (id: "always", result: Asset("a/2.clip.ron")),
                   (id: "dead", cells: { "c": False }, result: Asset("a/3.clip.ron"))])"#,
    );
    let issues = t.validate(&NoChoosers);
    let dead = first_issue(&issues, "chooser_unreachable_row");
    assert_eq!(
        (dead.row.as_deref(), dead.severity),
        (Some("dead"), IssueSeverity::Warning)
    );
    assert!(
        !codes(&issues).contains(&"chooser_no_catch_all"),
        "`always` is a catch-all row"
    );
    let gated = table(
        r#"(format_version: 1, result: Asset("clip"), inputs: { "b": Bool },
            columns: [(id: "c", input: "b", kind: Bool)],
            rows: [(id: "only", cells: { "c": True }, result: Asset("a/1.clip.ron"))])"#,
    );
    let info = first_issue(&gated.validate(&NoChoosers), "chooser_no_catch_all");
    assert_eq!(info.severity, IssueSeverity::Info);
    // Versions and unknown fields.
    assert_eq!(
        codes(&load_err(r#"(format_version: 2, result: OutputOnly)"#)),
        ["chooser_unsupported_version"]
    );
    assert_eq!(
        codes(&load_err(
            r#"(format_version: 1, result: OutputOnly, colums: [])"#
        )),
        ["chooser_bad_file"]
    );
}

// Test 15: trace contents.

#[test]
fn a_full_trace_lists_every_row_and_column() {
    let t = table(&MODES.replace("MODE", "Best"));
    let mut c = ctx(&[("speed", ChooserValue::Float(4.0))]);
    let (outcome, trace) = t.evaluate_traced(&NoChoosers, &mut c, &mut Rng::new(0), None, TraceDetail::Full);
    let selected = outcome.and_then(|o| o.row);
    assert_eq!(trace.selected, selected);
    assert_eq!(trace.rows.len(), 5);
    assert_eq!(trace.mode, Mode::Best);
    assert_eq!(trace.inputs, [("speed".to_owned(), ChooserValue::Float(4.0))]);
    for row in &trace.rows {
        if row.enabled {
            assert_eq!(row.cells.len(), 2, "row {} lists both columns", row.id);
        }
    }
    let r0 = &trace.rows[0];
    assert_eq!(r0.outcome, RowOutcome::RejectedBy("fast".into()));
    assert_eq!(r0.cells[0].verdict, Verdict::Fail);
    assert_eq!(r0.cells[0].detail, "input 4 not in [5, 100]");
    assert!(
        matches!(r0.cells[1].verdict, Verdict::Score(_)),
        "full detail scores the rejected row too"
    );
    assert_eq!(trace.rows[3].outcome, RowOutcome::Disabled);
    assert_eq!(trace.draw, None, "no draw outside Weighted");
    let (_, brief) = t.evaluate_traced(&NoChoosers, &mut c, &mut Rng::new(0), None, TraceDetail::Brief);
    assert_eq!(
        brief.rows[0].cells[1].verdict,
        Verdict::Skipped,
        "brief stops at the first failure"
    );
    let w = table(WEIGHTED);
    let (_, trace) = w.evaluate_traced(&NoChoosers, &mut c, &mut Rng::new(0), None, TraceDetail::Brief);
    assert!(trace.draw.is_some());
}

#[test]
fn a_traced_evaluation_records_no_recent_pick() {
    let mut t = table(WEIGHTED);
    t.recency = Some(Recency {
        window: 1,
        penalty: 0.0,
    });
    let mut mem = RecentPicks::default();
    let _ = t.evaluate_traced(
        &NoChoosers,
        &mut ChooserContext::default(),
        &mut Rng::new(0),
        Some(&mut mem),
        TraceDetail::Brief,
    );
    assert!(mem.rows.is_empty());
    let _ = t.evaluate(
        &NoChoosers,
        &mut ChooserContext::default(),
        &mut Rng::new(0),
        Some(&mut mem),
    );
    assert_eq!(mem.rows.len(), 1);
}

// RON round trip.

#[test]
fn ron_round_trip_is_byte_identical() {
    for text in [SPEC_EXAMPLE, WEIGHTED] {
        let t = table(text);
        let written = t.to_ron_string();
        let back = table(&written);
        assert_eq!(back, t);
        assert_eq!(back.to_ron_string(), written);
    }
    let rich = table(
        r#"(format_version: 1, mode: Weighted, result: Value(Tags),
            inputs: { "e": Enum(["x", "y"]), "n": Name, "t": Tags, "a": Asset, "i": Int, "f": Float },
            outputs: { "o": Tags, "s": Enum(["p", "q"]) },
            columns: [
                (id: "e", input: "e", kind: Enum), (id: "n", input: "n", kind: Name, case_sensitive: false),
                (id: "t", input: "t", kind: Tags), (id: "a", input: "a", kind: Asset),
                (id: "i", input: "i", kind: IntRange, wrap: Some((0.0, 24.0)), disabled: true),
                (id: "f", input: "f", kind: FloatDistance(max_distance: 2.5, wrap: Some((0.0, 1.0)), drop_beyond: true)),
                (id: "w", kind: Weight), (id: "o", output: "o", kind: SetTags), (id: "s", output: "s", kind: SetEnum),
            ],
            rows: [
                (id: "r", cells: { "e": OneOf(["x", "y"]), "n": "Bob", "t": Has("A.B"), "a": Kind("clip"),
                    "i": (22, 4), "f": Any, "w": 0.5, "o": ["A.B", "C"], "s": "q" },
                 result: Value(["X.Y"]), note: "a note"),
                (id: "s", enabled: false, result: Inline((result: Value(Tags), rows: [(id: "in", result: Nothing)]))),
            ],
            near_best: Some(0.25), recency: Some((window: 2, penalty: 0.5)))"#,
    );
    let written = rich.to_ron_string();
    let back = table(&written);
    assert_eq!(back, rich);
    assert_eq!(back.to_ron_string(), written);
    assert!(chooser_migration_sources().is_empty());
    assert!(chooser_schema_fingerprint().contains("catch_all"));
}

// Test 19 (chooser half): a bad reload keeps the previous table and shows the error.

#[test]
fn a_bad_chooser_reload_keeps_the_previous_table() {
    let mut choosers = Choosers::default();
    let path = "s/w.chooser.ron";
    assert_eq!(choosers.load_text(path, WEIGHTED), Ok(1));
    let issues = choosers
        .load_text(path, &WEIGHTED.replace("2.0", "-2.0"))
        .expect_err("negative weight");
    assert_eq!(issues[0].code, "chooser_bad_number");
    assert_eq!(choosers.generation(path), 1);
    assert_eq!(choosers.get(path).map(|t| t.rows.len()), Some(3));
    let slot = choosers.slot(path).expect("slot");
    assert_eq!(slot.rejected_reloads, 1);
    assert_eq!(slot.last_issues[0].code, "chooser_bad_number");
}

#[test]
fn huge_finite_weights_still_split_the_draw() {
    // Two rows of weight 2e38: an f32 total is infinite and every draw took the last row
    // (ADR-1809). Summed in f64 the draw splits about evenly.
    let t = table(
        r#"(format_version: 1, mode: Weighted, result: Asset("cue"),
            columns: [(id: "w", kind: Weight)],
            rows: [
                (id: "a", cells: { "w": 2e38 }, result: Asset("s/a.cue.ron")),
                (id: "b", cells: { "w": 2e38 }, result: Asset("s/b.cue.ron")),
            ])"#,
    );
    let mut rng = Rng::new(11);
    let mut c = ChooserContext::default();
    let a = (0..400)
        .filter(|_| {
            t.evaluate(&NoChoosers, &mut c, &mut rng, None)
                .and_then(|o| o.row)
                .as_deref()
                == Some("a")
        })
        .count();
    assert!((140..=260).contains(&a), "row a won {a} of 400");
}

#[test]
fn evaluate_all_drops_a_catch_all_expansion_that_nests_too_deep() {
    // The catch-all expands `c/x`, whose first row is a plain asset and whose second nests nine
    // levels: the expansion fails as a whole, as `evaluate` does (ADR-1809).
    let mut env = chain(10);
    env.insert(
        "c/x.chooser.ron".to_owned(),
        table(
            r#"(format_version: 1, result: Asset("clip"), rows: [
                (id: "early", result: Asset("a/early.clip.ron")),
                (id: "deep", result: Chooser("c/1.chooser.ron")),
            ])"#,
        ),
    );
    let top = table(
        r#"(format_version: 1, result: Asset("clip"), rows: [],
            catch_all: Some((result: Chooser("c/x.chooser.ron"))))"#,
    );
    let all = top.evaluate_all(&env, &mut ChooserContext::default());
    assert!(all.is_empty(), "no partial expansion: {all:?}");
}

#[test]
fn framework2_perf_chooser_clones() {
    use regia_app::prelude::*;

    fn provider_v(_: &regia_app::prelude::World, _: regia_app::prelude::Entity, ctx: &mut ChooserContext) {
        ctx.set("v", ChooserValue::Int(7));
    }
    fn provider_w(_: &regia_app::prelude::World, _: regia_app::prelude::Entity, ctx: &mut ChooserContext) {
        ctx.set("w", ChooserValue::Int(1));
    }
    let table = ChooserTable::from_ron_str(
        r#"(format_version: 1, mode: First, result: Asset("clip"), inputs: { "v": Int, "w": Int },
            columns: [(id: "c", input: "v", kind: IntRange)],
            rows: [(id: "ok", cells: { "c": (0, 9) }, result: Asset("a/ok.clip.ron"))])"#,
    )
    .expect("table");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ChooserPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.register_chooser_provider("pv", provider_v);
    app.register_chooser_provider("pw", provider_w);
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("t.chooser.ron", table);
    app.world_mut()
        .spawn(ChooserBinding::new("t.chooser.ron", "pv", "site-v"));
    app.world_mut()
        .spawn(ChooserBinding::new("t.chooser.ron", "pw", "site-w"));
    let providers0 = super::runtime::provider_map_clones();
    let paths0 = super::runtime::table_path_clones();
    let keys0 = super::context_key_inserts();
    app.update();
    let stats = *app.world().resource::<ChooserStats>();
    assert_eq!(stats.evaluations, 2, "each binding evaluates once");
    let providers = super::runtime::provider_map_clones();
    let paths = super::runtime::table_path_clones();
    let keys = super::context_key_inserts();
    assert_eq!(
        providers, providers0,
        "the first pass clones the provider map ({providers0} -> {providers})"
    );
    assert!(
        paths >= paths0 + 2,
        "the first pass clones each table path ({paths0} -> {paths})"
    );
    assert!(
        keys >= keys0 + 2,
        "the first pass inserts context keys ({keys0} -> {keys})"
    );
    app.update();
    let stats = *app.world().resource::<ChooserStats>();
    assert_eq!(stats.evaluations, 2, "unchanged bindings stay evaluated once");
    assert!(stats.skipped >= 2, "the second pass skips both bindings");
    let providers2 = super::runtime::provider_map_clones();
    let paths2 = super::runtime::table_path_clones();
    let keys2 = super::context_key_inserts();
    assert!(
        providers2 == providers && paths2 == paths && keys2 == keys,
        "unchanged pass cloned again: providers {providers}->{providers2}, paths {paths}->{paths2}, keys {keys}->{keys2}"
    );
}

/// corr-low-framework (corr-framework-a L7): the per-entity scratch contexts of despawned bound
/// entities are released even when no binding is left. The early return for "no bound entity"
/// skipped the prune, so the last contexts stayed until a binding existed again.
#[test]
fn the_scratch_contexts_go_when_the_last_binding_does() {
    use regia_app::prelude::*;

    fn provider_v(_: &regia_app::prelude::World, _: regia_app::prelude::Entity, ctx: &mut ChooserContext) {
        ctx.set("v", ChooserValue::Int(7));
    }
    let table = ChooserTable::from_ron_str(
        r#"(format_version: 1, mode: First, result: Asset("clip"), inputs: { "v": Int },
            columns: [(id: "c", input: "v", kind: IntRange)],
            rows: [(id: "ok", cells: { "c": (0, 9) }, result: Asset("a/ok.clip.ron"))])"#,
    )
    .expect("table");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ChooserPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.register_chooser_provider("pv", provider_v);
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("t.chooser.ron", table);
    let bound = app
        .world_mut()
        .spawn(ChooserBinding::new("t.chooser.ron", "pv", "site-v"))
        .id();
    app.update();
    assert_eq!(
        super::runtime::binding_scratch_len(app.world()),
        1,
        "one context while bound"
    );
    app.world_mut().despawn(bound);
    app.update();
    app.update();
    assert_eq!(
        super::runtime::binding_scratch_len(app.world()),
        0,
        "the despawned entity's context is released"
    );
}

#[test]
fn integer_ranges_compare_exactly_past_two_to_the_53() {
    // min 2^53 + 1: the input 2^53 is one below it and must fail (as f64 both are 2^53).
    let t = gate_table(
        "IntRange",
        "Int",
        &[("big", "(9007199254740993, 9007199254740994)")],
        "",
    );
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Int(9_007_199_254_740_992))])),
        None
    );
    assert_eq!(
        pick(&t, &mut ctx(&[("v", ChooserValue::Int(9_007_199_254_740_993))])).as_deref(),
        Some("big")
    );
}

// The cooked (pak) form, spec 51 § 4.4 (ADR-2012).

const COOKED_FIXTURE: &str = r#"(
    format_version: 1,
    mode: Weighted,
    result: Asset("montage"),
    inputs: { "armed": Bool, "ammo": Int, "angle": Float, "stance": Enum(["stand", "crouch"]),
              "who": Name, "tags": Tags, "held": Asset },
    outputs: { "rate": Float, "take": Enum(["a", "b"]), "note": Name, "n": Int, "ok": Bool,
               "set": Tags, "fx": Asset },
    columns: [
        (id: "armed", input: "armed", kind: Bool),
        (id: "ammo", input: "ammo", kind: IntRange, wrap: Some((0.0, 8.0))),
        (id: "turn", input: "angle", kind: FloatRange, wrap: Some((-180.0, 180.0))),
        (id: "stance", input: "stance", kind: Enum),
        (id: "who", input: "who", kind: Name, case_sensitive: false),
        (id: "tags", input: "tags", kind: Tags),
        (id: "held", input: "held", kind: Asset),
        (id: "near", input: "angle", kind: FloatDistance(influence: 2.0, max_distance: 90.0,
            wrap: Some((-180.0, 180.0)), drop_beyond: true)),
        (id: "weight", kind: Weight),
        (id: "rate", output: "rate", kind: SetFloat),
        (id: "take", output: "take", kind: SetEnum),
        (id: "note", output: "note", kind: SetName),
        (id: "n", output: "n", kind: SetInt),
        (id: "ok", output: "ok", kind: SetBool),
        (id: "set", output: "set", kind: SetTags),
        (id: "fx", output: "fx", kind: SetAsset),
        (id: "off", input: "armed", kind: Bool, disabled: true),
    ],
    rows: [
        (id: "front", cells: { "armed": True, "ammo": (6, 2), "turn": (170.0, -170.0),
            "stance": Is("stand"), "who": "Ada", "held": Is("a/b.prefab.ron"), "near": 0.0,
            "weight": 2.5, "rate": 1.25, "take": "a", "note": "hi", "n": -3, "ok": true,
            "set": ["A.B", "C"], "fx": "fx/hit.prefab.ron",
            "tags": AllOf([Has("A"), NoneOf([HasExact("B.C"), AnyOf([Has("D")])])]) },
         result: Asset("anim/front.montage.ron"), note: "the common hit"),
        (id: "back", enabled: false, cells: { "armed": False, "stance": IsNot("crouch"),
            "held": Kind("prefab") }, result: Chooser("anim/back.chooser.ron")),
        (id: "side", cells: { "stance": OneOf(["stand", "crouch"]), "near": Any, "weight": 0.5 },
         result: Inline((
            mode: Best,
            result: Asset("montage"),
            inputs: { "angle": Float },
            columns: [(id: "a", input: "angle", kind: FloatDistance(max_distance: 10.0))],
            rows: [(id: "deep", cells: { "a": 1.5 }, result: Inline((
                result: Asset("montage"),
                rows: [(id: "deepest", cells: {}, result: Asset("anim/deepest.montage.ron"))],
            )))],
            catch_all: Some((result: Asset("anim/inner.montage.ron"))),
         ))),
    ],
    catch_all: Some((result: Asset("anim/generic.montage.ron"),
                     outputs: { "rate": 1.0, "set": ["A"], "take": "b" })),
    near_best: Some(0.25),
    recency: Some((window: 3, penalty: 0.25)),
)"#;

#[test]
fn a_cooked_chooser_is_the_table_it_was_cooked_from() {
    let source = table(COOKED_FIXTURE);
    let cooked = source.to_cooked().expect("cook");
    assert_eq!(cooked[..4], CHOOSER_COOKED_VERSION.to_le_bytes());
    let loaded = ChooserTable::from_cooked(&cooked).expect("load");
    assert_eq!(loaded, source);
    assert_eq!(loaded.to_ron_string(), source.to_ron_string());
}

#[test]
fn a_cooked_chooser_evaluates_like_its_source() {
    let source = table(COOKED_FIXTURE);
    let loaded = ChooserTable::from_cooked(&source.to_cooked().expect("cook")).expect("load");
    for (seed, angle) in [(1, 0.0), (2, 45.0), (3, 179.0), (4, -91.0), (7, 180.0)] {
        let inputs = [
            ("armed", ChooserValue::Bool(true)),
            ("ammo", ChooserValue::Int(7)),
            ("angle", ChooserValue::Float(angle)),
            ("stance", ChooserValue::Enum("stand".into())),
            ("who", ChooserValue::Name("ADA".into())),
            (
                "tags",
                ChooserValue::Tags(TagContainer::from_names(["A.X"]).expect("tags")),
            ),
            ("held", ChooserValue::Asset("a/b.prefab.ron".into())),
        ];
        let (mut a, mut b) = (ctx(&inputs), ctx(&inputs));
        let (mut rng_a, mut rng_b) = (Rng::new(seed), Rng::new(seed));
        let (outcome_a, trace_a) =
            source.evaluate_traced(&NoChoosers, &mut a, &mut rng_a, None, TraceDetail::Full);
        let (outcome_b, trace_b) =
            loaded.evaluate_traced(&NoChoosers, &mut b, &mut rng_b, None, TraceDetail::Full);
        assert_eq!(outcome_a, outcome_b, "seed {seed}");
        assert_eq!(trace_a, trace_b, "seed {seed}");
        assert_eq!(a, b, "outputs, seed {seed}");
    }
}

#[test]
fn cooking_a_chooser_is_stable_for_the_same_table() {
    let first = table(COOKED_FIXTURE).to_cooked().expect("cook");
    let again = ChooserTable::from_cooked(&first)
        .expect("load")
        .to_cooked()
        .expect("cook");
    assert_eq!(first, again, "a cache key may rest on these bytes");
}

#[test]
fn an_unknown_cooked_chooser_version_is_refused_by_name() {
    let cooked = table(COOKED_FIXTURE).to_cooked().expect("cook");
    for version in [0, CHOOSER_COOKED_VERSION + 1, u32::MAX] {
        let mut bytes = version.to_le_bytes().to_vec();
        bytes.extend_from_slice(&cooked[4..]);
        let error = ChooserTable::from_cooked(&bytes).expect_err("refused");
        assert_eq!(
            codes(&error.issues),
            ["chooser_unsupported_version"],
            "version {version}"
        );
    }
}

#[test]
fn every_truncation_of_a_cooked_chooser_is_an_error_and_never_a_panic() {
    let cooked = table(COOKED_FIXTURE).to_cooked().expect("cook");
    for len in 0..cooked.len() {
        let error = ChooserTable::from_cooked(&cooked[..len]).expect_err("truncated");
        assert_eq!(codes(&error.issues), ["chooser_bad_file"], "length {len}");
    }
    let mut trailing = cooked;
    trailing.push(0);
    let error = ChooserTable::from_cooked(&trailing).expect_err("trailing byte");
    assert_eq!(codes(&error.issues), ["chooser_bad_file"]);
}

#[test]
fn flipped_bytes_in_a_cooked_chooser_never_panic() {
    let cooked = table(COOKED_FIXTURE).to_cooked().expect("cook");
    let mut rng = Rng::new(11);
    for _ in 0..4_000 {
        let mut bytes = cooked.clone();
        for _ in 0..3 {
            let at = 4 + (rng.next_u32() as usize) % (bytes.len() - 4);
            bytes[at] ^= 1 << (rng.next_u32() % 8);
        }
        // Either outcome is fine; a table that loads must have passed the checks.
        if let Ok(loaded) = ChooserTable::from_cooked(&bytes) {
            assert!(!loaded.validate_structure().iter().any(ChooserIssue::is_error));
        }
    }
}

/// A cooked payload is not trusted: what the text path refuses stays refused.
#[test]
fn a_cooked_chooser_that_breaks_a_rule_is_refused_by_the_rule_s_own_code() {
    let broken = |edit: fn(&mut ChooserTable)| {
        let mut source = table(COOKED_FIXTURE);
        edit(&mut source);
        let bytes = source.to_cooked().expect("the encoder does not validate");
        codes(&ChooserTable::from_cooked(&bytes).expect_err("refused").issues)
    };
    assert!(broken(|t| t.rows[1].id = "front".into()).contains(&"chooser_duplicate_row"));
    assert!(broken(|t| t.columns[0].input = Some("nope".into())).contains(&"chooser_unknown_input"));
    assert!(
        broken(|t| {
            t.rows[0].cells.insert("armed".into(), Cell::Weight(1.0));
        })
        .contains(&"chooser_bad_cell")
    );
    assert!(
        broken(|t| {
            t.rows[0].cells.insert("weight".into(), Cell::Weight(-1.0));
        })
        .contains(&"chooser_bad_number")
    );
    assert!(broken(|t| t.near_best = Some(f32::NAN)).contains(&"chooser_bad_number"));
    assert!(
        broken(|t| t.result_type = ResultType::Value(ScalarKind::Bool))
            .contains(&"chooser_result_type_mismatch")
    );
}

/// `validate_structure` does not look for this; only the text parser refused it.
#[test]
fn a_cooked_nan_value_is_refused_like_text_refuses_it() {
    let issues = ChooserTable::from_cooked(&cooked::hostile::nan_value_payload())
        .expect_err("NaN result")
        .issues;
    assert_eq!(codes(&issues), ["chooser_bad_number"]);
    let text = r#"(format_version: 1, result: Value(Float),
        rows: [(id: "r", cells: {}, result: Value(NaN))])"#;
    assert!(!codes(&load_err(text)).is_empty(), "the text form refuses it too");
}

#[test]
fn inline_tables_must_each_be_used_once_and_only_by_a_later_table() {
    use cooked::hostile::inline_payload;
    let code = |bytes: &[u8]| codes(&ChooserTable::from_cooked(bytes).expect_err("refused").issues);
    assert_eq!(
        code(&inline_payload(2, &[0, 0])),
        ["chooser_bad_file"],
        "used twice"
    );
    assert_eq!(
        code(&inline_payload(1, &[0])),
        ["chooser_bad_file"],
        "names itself"
    );
    assert_eq!(
        code(&inline_payload(2, &[1])),
        ["chooser_bad_file"],
        "names a later table"
    );
    assert_eq!(code(&inline_payload(1, &[99])), ["chooser_bad_file"], "dangling");
    assert_eq!(
        code(&inline_payload(3, &[0])),
        ["chooser_bad_file"],
        "a table nothing uses"
    );
    assert_eq!(code(&inline_payload(0, &[])), ["chooser_bad_file"], "no table");
    assert!(ChooserTable::from_cooked(&inline_payload(2, &[0])).is_ok());
}

/// A million nested tables must fail on the count, not on the stack: the load is a flat loop.
#[test]
fn inline_nesting_is_bounded_without_recursion() {
    use cooked::hostile::nested_payload;
    assert!(ChooserTable::from_cooked(&nested_payload(MAX_COOKED_NESTING + 1)).is_ok());
    for depth in [MAX_COOKED_NESTING + 2, 1_000_000] {
        let error = ChooserTable::from_cooked(&nested_payload(depth)).expect_err("too deep");
        assert_eq!(codes(&error.issues), ["chooser_depth_exceeded"], "depth {depth}");
    }
}

#[test]
fn tag_queries_in_a_cooked_chooser_keep_their_limits() {
    use cooked::hostile::row_cell_payload;
    let code = |bytes: &[u8]| codes(&ChooserTable::from_cooked(bytes).expect_err("refused").issues);
    assert!(ChooserTable::from_cooked(&row_cell_payload(0, vec![(true, 0), (true, 0), (false, 2)])).is_ok());
    assert_eq!(
        code(&row_cell_payload(1, vec![(true, 0)])),
        ["chooser_bad_file"],
        "no such column"
    );
    assert_eq!(
        code(&row_cell_payload(0, vec![(false, 1)])),
        ["chooser_bad_file"],
        "too few children"
    );
    assert_eq!(
        code(&row_cell_payload(0, vec![(false, u32::MAX)])),
        ["chooser_bad_file"]
    );
    assert_eq!(
        code(&row_cell_payload(0, Vec::new())),
        ["chooser_bad_file"],
        "no nodes"
    );
    assert_eq!(
        code(&row_cell_payload(0, vec![(true, 0), (true, 0)])),
        ["chooser_bad_file"],
        "two roots"
    );
    let deep: Vec<(bool, u32)> = std::iter::once((true, 0))
        .chain((0..MAX_QUERY_DEPTH).map(|_| (false, 1)))
        .collect();
    assert_eq!(code(&row_cell_payload(0, deep)), ["chooser_bad_file"], "too deep");
    let wide = vec![(true, 0); MAX_QUERY_NODES + 1];
    assert_eq!(
        code(&row_cell_payload(0, wide)),
        ["chooser_bad_file"],
        "too many nodes"
    );
}

#[test]
fn a_cooked_chooser_loads_through_the_slot_and_says_where_it_came_from() {
    let mut choosers = Choosers::default();
    let cooked = table(COOKED_FIXTURE).to_cooked().expect("cook");
    assert_eq!(choosers.load_cooked("t.chooser.ron", &cooked).expect("load"), 1);
    assert!(choosers.slot("t.chooser.ron").expect("slot").cooked);
    assert_eq!(choosers.get("t.chooser.ron"), Some(&table(COOKED_FIXTURE)));
    choosers
        .load_text("t.chooser.ron", COOKED_FIXTURE)
        .expect("reload");
    assert!(!choosers.slot("t.chooser.ron").expect("slot").cooked);
    let generation = choosers.generation("t.chooser.ron");
    let issues = choosers
        .load_cooked("t.chooser.ron", b"\x01\0\0\0junk")
        .expect_err("refused");
    assert_eq!(issues[0].code, "chooser_bad_file");
    assert_eq!(choosers.generation("t.chooser.ron"), generation);
    assert_eq!(choosers.get("t.chooser.ron"), Some(&table(COOKED_FIXTURE)));
}

#[test]
fn the_result_tag_names_what_the_table_yields() {
    assert_eq!(table(COOKED_FIXTURE).result_tag(), "montage");
    assert_eq!(
        ChooserTable::new(Mode::First, ResultType::OutputOnly).result_tag(),
        "output_only"
    );
    assert_eq!(
        ChooserTable::new(Mode::First, ResultType::Value(ScalarKind::Name)).result_tag(),
        "name"
    );
}

#[test]
fn a_table_lists_the_assets_it_names_for_the_cook() {
    let references = table(COOKED_FIXTURE).asset_references();
    assert_eq!(
        references,
        [
            "a/b.prefab.ron",
            "anim/back.chooser.ron",
            "anim/deepest.montage.ron",
            "anim/front.montage.ron",
            "anim/generic.montage.ron",
            "anim/inner.montage.ron",
            "fx/hit.prefab.ron",
        ]
    );
    assert_eq!(table(COOKED_FIXTURE).nested_choosers(), ["anim/back.chooser.ron"]);
}

/// `RowCells` against the map it replaced (ADR-2012): one fixed walk of inserts, overwrites and
/// removes over a few columns, comparing every answer and the order after each step.
#[test]
fn row_cells_answer_as_a_map_does() {
    let mut cells = RowCells::new();
    let mut map: BTreeMap<String, Cell> = BTreeMap::new();
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    for step in 0..5_000_u32 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let column = format!("c{}", state % 17);
        if state & (1 << 40) == 0 {
            let cell = Cell::Weight(step as f32);
            assert_eq!(
                cells.insert(column.clone(), cell.clone()),
                map.insert(column.clone(), cell)
            );
        } else {
            assert_eq!(cells.remove(&column), map.remove(&column));
        }
        assert_eq!(cells.get(&column), map.get(&column));
        assert_eq!(cells.contains_key(&column), map.contains_key(&column));
        assert_eq!(cells.len(), map.len());
        assert!(
            cells.iter().eq(map.iter()),
            "same pairs in the same order at {step}"
        );
    }
    assert!(!cells.is_empty(), "the walk must end with cells");
    assert_eq!(RowCells::from(map.clone()), cells);
    assert_eq!(map.clone().into_iter().collect::<RowCells>(), cells);
}

#[test]
fn row_cells_collect_keeps_the_last_of_a_repeated_column() {
    let cells: RowCells = [
        ("b".to_owned(), Cell::Weight(1.0)),
        ("a".to_owned(), Cell::Weight(2.0)),
        ("b".to_owned(), Cell::Weight(3.0)),
    ]
    .into_iter()
    .collect();
    assert_eq!(cells.len(), 2);
    assert_eq!(cells.get("b"), Some(&Cell::Weight(3.0)));
    assert_eq!(cells.iter().next().map(|(column, _)| column.as_str()), Some("a"));
}

/// A cooked row read whole refuses a repeated column by name.
#[test]
fn row_cells_read_whole_refuses_a_repeated_column() {
    let cells = vec![
        ("b".to_owned(), Cell::Any),
        ("a".to_owned(), Cell::Any),
        ("b".to_owned(), Cell::Any),
    ];
    assert_eq!(RowCells::from_unsorted(cells), Err("b".to_owned()));
}

// ---- Spec-conformance lane (spec 51 audit, 2026-11-10): rules that had no test. ----

/// § 2.5: NaN anywhere is `chooser_bad_number`: in a `Value(..)` result, in an emit cell and in a
/// catch-all output. The text loader reported the first as `chooser_result_type_mismatch` and the
/// other two as `chooser_bad_cell`. A table built in code is caught by `validate_structure`.
#[test]
fn nan_values_are_chooser_bad_number_wherever_they_sit() {
    let result = load_err(
        r#"(format_version: 1, result: Value(Float),
        rows: [(id: "r", cells: {}, result: Value(NaN))])"#,
    );
    assert_eq!(
        first_issue(&result, "chooser_bad_number").row.as_deref(),
        Some("r")
    );
    assert!(
        !codes(&result).contains(&"chooser_result_type_mismatch"),
        "{result:?}"
    );

    let emit = load_err(&fixture(
        r#"(id: "o", output: "o", kind: SetFloat)"#,
        r#"(id: "r", cells: { "o": NaN }, result: Asset("a/r.clip.ron"))"#,
        "",
    ));
    let nan = first_issue(&emit, "chooser_bad_number");
    assert_eq!(
        (nan.row.as_deref(), nan.column.as_deref()),
        (Some("r"), Some("o"))
    );
    assert!(!codes(&emit).contains(&"chooser_bad_cell"), "{emit:?}");

    let catch = load_err(
        r#"(format_version: 1, result: Asset("clip"), outputs: { "o": Float },
        rows: [(id: "r", result: Asset("a/r.clip.ron"))],
        catch_all: Some((result: Asset("a/c.clip.ron"), outputs: { "o": NaN })))"#,
    );
    let nan = first_issue(&catch, "chooser_bad_number");
    assert_eq!(
        (nan.row.as_deref(), nan.column.as_deref()),
        (Some("catch_all"), Some("o"))
    );
    assert!(!codes(&catch).contains(&"chooser_bad_cell"), "{catch:?}");

    let mut t = table(
        r#"(format_version: 1, result: Value(Float), outputs: { "o": Float },
        rows: [(id: "r", result: Value(1.0))],
        catch_all: Some((result: Value(2.0), outputs: { "o": 0.5 })))"#,
    );
    assert!(t.validate_structure().iter().all(|i| !i.is_error()));
    t.rows[0].result = ResultEntry::Value(ChooserValue::Float(f32::NAN));
    t.catch_all = Some(CatchAll {
        result: ResultEntry::Value(ChooserValue::Float(f32::NAN)),
        outputs: [("o".to_owned(), ChooserValue::Float(f32::NAN))].into(),
    });
    let issues = t.validate_structure();
    assert_eq!(
        codes(&issues),
        ["chooser_bad_number", "chooser_bad_number", "chooser_bad_number"],
        "{issues:?}"
    );
}

/// § 2.5 rows without a fixture in `every_validation_code_has_a_fixture`: recency bounds,
/// `near_best` NaN, a second `Weight` column, an integer range, an emit column naming an
/// undeclared output, and result entries that disagree with `OutputOnly` / `Value(Int)`.
#[test]
fn more_validation_fixtures() {
    let row = r#"(id: "r", result: Asset("a/r.clip.ron"))"#;
    for extra in [
        ", recency: Some((window: 0, penalty: 0.5))",
        ", recency: Some((window: 17, penalty: 0.5))",
        ", recency: Some((window: 2, penalty: 1.5))",
        ", recency: Some((window: 2, penalty: -0.1))",
        ", near_best: Some(NaN)",
    ] {
        assert!(
            codes(&load_err(&fixture("", row, extra))).contains(&"chooser_bad_number"),
            "{extra}"
        );
    }
    for extra in [
        ", recency: Some((window: 1, penalty: 0.0))",
        ", recency: Some((window: 16, penalty: 1.0))",
    ] {
        assert!(
            ChooserTable::from_ron_str(&fixture("", row, extra)).is_ok(),
            "{extra}"
        );
    }
    let weights = first_issue(
        &load_err(&fixture(
            r#"(id: "w1", kind: Weight), (id: "w2", kind: Weight)"#,
            row,
            "",
        )),
        "chooser_kind_mismatch",
    );
    assert_eq!(weights.column.as_deref(), Some("w2"));
    let range = first_issue(
        &load_err(
            r#"(format_version: 1, result: Asset("clip"), inputs: { "i": Int },
            columns: [(id: "c", input: "i", kind: IntRange)],
            rows: [(id: "r", cells: { "c": (5, 1) }, result: Asset("a/r.clip.ron"))])"#,
        ),
        "chooser_bad_range",
    );
    assert_eq!(
        (range.row.as_deref(), range.column.as_deref()),
        (Some("r"), Some("c"))
    );
    let output = first_issue(
        &load_err(&fixture(r#"(id: "s", output: "nope", kind: SetFloat)"#, row, "")),
        "chooser_unknown_input",
    );
    assert_eq!(output.column.as_deref(), Some("s"));
    let output_only = first_issue(
        &load_err(
            r#"(format_version: 1, result: OutputOnly, rows: [(id: "r", result: Asset("a/r.clip.ron"))])"#,
        ),
        "chooser_result_type_mismatch",
    );
    assert_eq!(output_only.row.as_deref(), Some("r"));
    let int = first_issue(
        &load_err(r#"(format_version: 1, result: Value(Int), rows: [(id: "r", result: Value("x"))])"#),
        "chooser_result_type_mismatch",
    );
    assert_eq!(int.row.as_deref(), Some("r"));
}

/// § 2.6: the trace log keeps the last 32 traces per site, oldest dropped; `limit` takes the newest.
#[test]
fn the_trace_log_keeps_the_last_32_per_site() {
    let mut log = ChooserTraceLog::default();
    let entry = |site: &str, frame: u64| LoggedTrace {
        site: site.to_owned(),
        frame,
        outcome: None,
        trace: ChooserTrace::default(),
    };
    for frame in 0..33 {
        log.push(entry("s", frame));
    }
    log.push(entry("other", 100));
    let kept: Vec<u64> = log.traces(Some("s"), 100).iter().map(|t| t.frame).collect();
    assert_eq!(kept.len(), ChooserTraceLog::PER_SITE);
    assert_eq!(ChooserTraceLog::PER_SITE, 32);
    assert_eq!(kept.first(), Some(&1), "frame 0 fell out of the ring");
    assert_eq!(kept.last(), Some(&32));
    let newest: Vec<u64> = log.traces(Some("s"), 3).iter().map(|t| t.frame).collect();
    assert_eq!(newest, [30, 31, 32]);
    assert_eq!(log.traces(None, 100).len(), 33, "every site");
}

/// § 2.4: `Best` costs within 1e-6 (absolute) count as equal and the earlier row wins; a larger
/// gap goes to the cheaper row.
#[test]
fn best_treats_costs_within_one_millionth_as_a_tie() {
    let t = |early: f32| {
        table(&format!(
            r#"(format_version: 1, mode: Best, result: Asset("clip"), inputs: {{ "v": Float }},
            columns: [(id: "d", input: "v", kind: FloatDistance(max_distance: 1.0))],
            rows: [
                (id: "early", cells: {{ "d": {early:?} }}, result: Asset("a/e.clip.ron")),
                (id: "late", cells: {{ "d": 0.0 }}, result: Asset("a/l.clip.ron")),
            ])"#
        ))
    };
    let v = || ctx(&[("v", ChooserValue::Float(0.0))]);
    assert_eq!(
        pick(&t(5e-7), &mut v()).as_deref(),
        Some("early"),
        "5e-7 apart is a tie"
    );
    assert_eq!(
        pick(&t(1e-5), &mut v()).as_deref(),
        Some("late"),
        "1e-5 apart is not"
    );
}

/// § 2.4: a negative `near_best` clamps to 0 (only the best cost takes part), it does not empty
/// the draw.
#[test]
fn a_negative_near_best_clamps_to_zero() {
    let t = table(
        r#"(format_version: 1, mode: Weighted, result: Asset("cue"), inputs: { "v": Float },
            columns: [(id: "d", input: "v", kind: FloatDistance(max_distance: 10.0)), (id: "w", kind: Weight)],
            rows: [
                (id: "far", cells: { "d": 9.0, "w": 100.0 }, result: Asset("s/far.cue.ron")),
                (id: "near", cells: { "d": 0.0, "w": 1.0 }, result: Asset("s/near.cue.ron")),
            ],
            near_best: Some(-1.0))"#,
    );
    let mut rng = Rng::new(3);
    for _ in 0..50 {
        let o = t
            .evaluate(
                &NoChoosers,
                &mut ctx(&[("v", ChooserValue::Float(0.0))]),
                &mut rng,
                None,
            )
            .expect("a pick");
        assert_eq!(o.row.as_deref(), Some("near"));
        assert!(!o.used_catch_all);
    }
}

/// § 2.4: a penalty in (0, 1) scales the weight of rows among the last `window` picks only, and
/// the memory records each pick.
#[test]
fn partial_recency_scales_only_the_rows_in_the_window() {
    let mut t = table(WEIGHTED);
    t.recency = Some(Recency {
        window: 2,
        penalty: 0.25,
    });
    // Memory oldest first: c is outside the window of 2, a and b are inside.
    let memory = RecentPicks {
        rows: ["c", "a", "b"].iter().map(|s| (*s).to_owned()).collect(),
    };
    let mut rng = Rng::new(21);
    let n = 4000;
    let mut counts = BTreeMap::<String, usize>::new();
    for _ in 0..n {
        let mut mem = memory.clone();
        let row = t
            .evaluate(
                &NoChoosers,
                &mut ChooserContext::default(),
                &mut rng,
                Some(&mut mem),
            )
            .and_then(|o| o.row)
            .expect("a pick");
        assert_eq!(mem.rows.len(), 4);
        assert_eq!(mem.rows.back(), Some(&row), "the pick is recorded");
        *counts.entry(row).or_default() += 1;
    }
    // Weights a 1 * 0.25, b 2 * 0.25, c 1: total 1.75.
    for (row, p) in [("a", 0.25 / 1.75), ("b", 0.5 / 1.75), ("c", 1.0 / 1.75)] {
        let got = counts.get(row).copied().unwrap_or(0) as f64;
        let expected = f64::from(n) * p;
        let sigma = (f64::from(n) * p * (1.0 - p)).sqrt();
        assert!(
            (got - expected).abs() <= 3.0 * sigma,
            "{row}: {got} vs {expected:.0}"
        );
    }
}

/// § 2.3 step 5 in `First` mode: a nested chooser that returns `None` strikes its row and the scan
/// goes on to the next candidate.
#[test]
fn first_mode_strikes_an_empty_nested_row_and_moves_on() {
    let env = nested_env();
    let t = table(
        r#"(format_version: 1, mode: First, result: Asset("clip"), inputs: { "go": Bool },
            columns: [(id: "go", input: "go", kind: Bool)],
            rows: [
                (id: "empty", cells: {}, result: Chooser("n/empty.chooser.ron")),
                (id: "gated", cells: { "go": True }, result: Asset("a/gated.clip.ron")),
                (id: "plain", cells: {}, result: Asset("a/plain.clip.ron")),
            ])"#,
    );
    let mut c = ctx(&[
        ("go", ChooserValue::Bool(false)),
        ("v", ChooserValue::Bool(false)),
    ]);
    let (o, trace) = t.evaluate_traced(&env, &mut c, &mut Rng::new(0), None, TraceDetail::Brief);
    let o = o.expect("plain");
    assert_eq!(o.row.as_deref(), Some("plain"));
    assert!(!o.used_catch_all);
    assert_eq!(trace.rows[0].outcome, RowOutcome::NestedEmpty);
    assert_eq!(trace.rows[2].outcome, RowOutcome::Selected);
}

/// § 2.3 step 5 / § 13 (ADR-1585) in `Weighted` mode: when the drawn row's nested chooser is empty
/// the row is struck and the draw repeats, costing one more `Rng::f32`; otherwise one draw.
#[test]
fn weighted_mode_redraws_after_an_empty_nested_row() {
    let env = nested_env();
    let t = table(
        r#"(format_version: 1, mode: Weighted, result: Asset("clip"),
            columns: [(id: "w", kind: Weight)],
            rows: [
                (id: "empty", cells: { "w": 1.0 }, result: Chooser("n/empty.chooser.ron")),
                (id: "other", cells: { "w": 1.0 }, result: Asset("a/other.clip.ron")),
            ])"#,
    );
    let (mut one, mut two) = (0, 0);
    for seed in 0..64 {
        let mut rng = Rng::new(seed);
        let start = rng.clone();
        let mut c = ctx(&[("v", ChooserValue::Bool(false))]);
        let (o, trace) = t.evaluate_traced(&env, &mut c, &mut rng, None, TraceDetail::Brief);
        assert_eq!(o.and_then(|o| o.row).as_deref(), Some("other"), "seed {seed}");
        let mut after_one = start.clone();
        let _ = after_one.f32();
        let mut after_two = after_one.clone();
        let _ = after_two.f32();
        if rng == after_one {
            one += 1;
            assert_ne!(trace.rows[0].outcome, RowOutcome::NestedEmpty, "seed {seed}");
        } else {
            assert_eq!(rng, after_two, "seed {seed}: one or two draws, never more");
            assert_eq!(trace.rows[0].outcome, RowOutcome::NestedEmpty, "seed {seed}");
            two += 1;
        }
    }
    assert!(
        one > 0 && two > 0,
        "both paths ran: {one} single, {two} double draws"
    );
}

/// § 2.3 step 5: an unresolved `Chooser` catch-all returns nothing (`None` from `evaluate`, an empty
/// list from `evaluate_all`).
#[test]
fn an_unresolved_chooser_catch_all_returns_nothing() {
    let t = table(
        r#"(format_version: 1, result: Asset("clip"), inputs: { "b": Bool },
            columns: [(id: "b", input: "b", kind: Bool)],
            rows: [(id: "r", cells: { "b": True }, result: Asset("a/r.clip.ron"))],
            catch_all: Some((result: Chooser("gone.chooser.ron"))))"#,
    );
    let mut c = ctx(&[("b", ChooserValue::Bool(false))]);
    assert_eq!(t.evaluate(&NoChoosers, &mut c, &mut Rng::new(0), None), None);
    assert!(t.evaluate_all(&NoChoosers, &mut c).is_empty());
    let issues = t.validate(&NoChoosers);
    assert!(codes(&issues).contains(&"chooser_ref_unresolved"), "{issues:?}");
}
