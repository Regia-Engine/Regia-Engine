// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 8 tests 1-6 and 19 (curve tables).

use regia_core::math::Rng;

use super::*;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-5 * (1.0 + b.abs())
}

fn keys(points: &[(f32, f32)]) -> Vec<CurveKey> {
    points.iter().map(|&(x, y)| CurveKey::new(x, y)).collect()
}

fn one(interp: TableInterp, points: &[(f32, f32)]) -> CurveTable {
    CurveTable::new(interp, vec![NamedCurve::new("c", keys(points))]).expect("valid table")
}

fn codes(error: &CurveTableError) -> Vec<&'static str> {
    error.issues.iter().map(|issue| issue.code).collect()
}

// Test 1: sampling.

#[test]
fn every_interpolation_returns_the_key_value_exactly_at_a_key() {
    let points = [(0.0, 1.0), (1.5, -2.0), (4.0, 7.25), (4.5, 7.5)];
    for interp in [TableInterp::Step, TableInterp::Linear, TableInterp::Cubic] {
        let table = one(interp, &points);
        for &(x, y) in &points {
            assert_eq!(table.sample_named("c", x), Some(y), "{interp:?} at x = {x}");
        }
    }
}

#[test]
fn linear_midpoint_is_the_mean_and_step_holds_the_left_key() {
    let linear = one(TableInterp::Linear, &[(0.0, 2.0), (10.0, 4.0)]);
    assert_eq!(linear.sample_named("c", 5.0), Some(3.0));
    let step = one(TableInterp::Step, &[(0.0, 2.0), (10.0, 4.0)]);
    assert_eq!(step.sample_named("c", 9.99), Some(2.0));
    assert_eq!(step.sample_named("c", 10.0), Some(4.0));
}

#[test]
fn non_uniform_spacing_is_respected() {
    let table = one(TableInterp::Linear, &[(0.0, 0.0), (1.0, 10.0), (11.0, 20.0)]);
    assert!(close(table.sample_named("c", 0.5).expect("curve"), 5.0));
    assert!(close(table.sample_named("c", 6.0).expect("curve"), 15.0));
}

#[test]
fn cubic_through_monotone_keys_never_overshoots() {
    let mut rng = Rng::new(51);
    for case in 0..200 {
        let n = 2 + (rng.next_u32() % 9) as usize;
        let mut x = 0.0_f32;
        let mut y = 0.0_f32;
        let rising = case % 2 == 0;
        let mut points = Vec::with_capacity(n);
        for _ in 0..n {
            points.push((x, y));
            // Non-uniform spacing; flat runs included.
            x += 0.1 + rng.f32() * 5.0;
            let step = if rng.f32() < 0.2 { 0.0 } else { rng.f32() * 10.0 };
            y += if rising { step } else { -step };
        }
        let table = one(TableInterp::Cubic, &points);
        for pair in points.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            let (lo, hi) = (y0.min(y1), y0.max(y1));
            let slack = 1e-4 * (1.0 + hi.abs());
            for i in 0..=16 {
                let sx = x0 + (x1 - x0) * (i as f32 / 16.0);
                let sy = table.sample_named("c", sx).expect("curve");
                assert!(
                    sy >= lo - slack && sy <= hi + slack,
                    "case {case}: y({sx}) = {sy} leaves [{lo}, {hi}]"
                );
            }
        }
    }
}

#[test]
fn explicit_tangents_win_over_derived_ones() {
    let mut points = keys(&[(0.0, 0.0), (1.0, 1.0)]);
    points[0].out_tangent = Some(0.0);
    points[1].in_tangent = Some(0.0);
    let flat_ends = CurveTable::new(TableInterp::Cubic, vec![NamedCurve::new("c", points)]).expect("table");
    // Smoothstep: 3t^2 - 2t^3 at t = 0.25.
    assert!(close(flat_ends.sample_named("c", 0.25).expect("curve"), 0.15625));
    let derived = one(TableInterp::Cubic, &[(0.0, 0.0), (1.0, 1.0)]);
    // Derived tangents of a straight two-key line are its slope, so cubic equals linear.
    assert!(close(derived.sample_named("c", 0.25).expect("curve"), 0.25));
}

/// ADR-2015, found by the `curves.cooked` fuzz target: a key at 1.8e-38 beside a step of 250 made the
/// tangent limiter divide into an f32 overflow, and the whole first two segments sampled NaN. Finite keys
/// sample finite values, also where a slope is steeper than f32 holds.
#[test]
fn derived_tangents_stay_finite_beside_a_tiny_or_a_huge_secant() {
    for points in [
        [(1.0, 0.0), (2.0, 1.836_709_9e-38), (3.0, 250.0)],
        [(0.0, 0.0), (1.0e-3, 3.0e38), (1.0, 3.0e38)],
        [(0.0, -3.0e38), (1.0e-30, 3.0e38), (1.0, 0.0)],
    ] {
        let table = one(TableInterp::Cubic, &points);
        for x in [-1.0, 0.0, 1.0e-31, 5.0e-4, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0] {
            let y = table.sample_named("c", x).expect("curve");
            assert!(!y.is_nan(), "{points:?} samples NaN at {x}");
        }
        for &(x, y) in &points {
            assert_eq!(table.sample_named("c", x), Some(y), "{points:?} at its key {x}");
        }
    }
    // The first case's limit is the monotone one: the rise from 0 to 1.8e-38 does not overshoot (within
    // the rounding of numbers this small).
    let table = one(
        TableInterp::Cubic,
        &[(1.0, 0.0), (2.0, 1.836_709_9e-38), (3.0, 250.0)],
    );
    for x in [1.25, 1.5, 1.75] {
        let y = table.sample_named("c", x).expect("curve");
        assert!((-1.0e-40..=1.837e-38).contains(&y), "{y} at {x}");
    }
}

#[test]
fn per_key_interpolation_overrides_the_curve_and_table() {
    let mut points = keys(&[(0.0, 0.0), (1.0, 1.0), (2.0, 3.0)]);
    points[1].interp = Some(TableInterp::Step);
    let table = CurveTable::new(TableInterp::Linear, vec![NamedCurve::new("c", points)]).expect("table");
    assert_eq!(table.sample_named("c", 0.5), Some(0.5));
    assert_eq!(table.sample_named("c", 1.5), Some(1.0));
}

#[test]
fn handles_skip_the_lookup_and_go_stale_on_structural_edits() {
    let mut table = one(TableInterp::Linear, &[(0.0, 0.0), (1.0, 1.0)]);
    let handle = table.handle("c").expect("handle");
    assert_eq!(table.sample(handle, 0.5), 0.5);
    assert_eq!(table.range(handle), (0.0, 1.0, 0.0, 1.0));
    table.set_key("c", CurveKey::new(2.0, 5.0)).expect("edit");
    assert_eq!(
        table.try_sample(handle, 2.0),
        Some(5.0),
        "a key edit keeps handles"
    );
    table
        .upsert_curve(NamedCurve::new("d", keys(&[(0.0, 9.0)])))
        .expect("add");
    assert_eq!(
        table.try_sample(handle, 0.5),
        None,
        "adding a curve makes old handles stale"
    );
    assert_eq!(table.sample(handle, 0.5), 0.0);
}

#[test]
fn a_non_finite_x_returns_the_first_key_and_is_counted() {
    let table = one(TableInterp::Linear, &[(1.0, 4.0), (2.0, 8.0)]);
    assert_eq!(table.sample_named("c", f32::NAN), Some(4.0));
    assert_eq!(table.sample_named("c", f32::INFINITY), Some(4.0));
    assert_eq!(table.take_nonfinite_counts(), vec![("c".to_owned(), 2)]);
    assert!(table.take_nonfinite_counts().is_empty());
}

// Test 2: extrapolation. Keys (0, 0), (10, 10), (20, 30): span 20, y_last - y_first = 30, slopes
// 1 (first segment) and 2 (last segment).

fn extrap_table(mode: Extrap) -> CurveTable {
    let mut curve = NamedCurve::new("c", keys(&[(0.0, 0.0), (10.0, 10.0), (20.0, 30.0)]));
    curve.pre = mode;
    curve.post = mode;
    CurveTable::new(TableInterp::Linear, vec![curve]).expect("table")
}

#[test]
fn every_extrapolation_mode_at_stated_points() {
    // (mode, y(first - 1), y(last + 1), y(last + 2.25 spans), y(last + 2.5 spans))
    let cases = [
        (Extrap::Hold, 0.0, 30.0, 30.0, 30.0),
        (Extrap::Linear, -1.0, 32.0, 120.0, 130.0),
        // x = -1 wraps to 19 (y 28); x = 21 to 1; x = 65 to 5; x = 70 to 10.
        (Extrap::Cycle, 28.0, 1.0, 5.0, 10.0),
        // Same, plus loops * 30: -1, +1, +3, +3 loops.
        (Extrap::CycleOffset, -2.0, 31.0, 95.0, 100.0),
        // Odd loops mirror: x = -1 reads 1, x = 21 reads 19, x = 65 reads 15, x = 70 reads 10.
        (Extrap::PingPong, 1.0, 28.0, 20.0, 10.0),
    ];
    for (mode, before, after, at_225, at_25) in cases {
        let table = extrap_table(mode);
        let y = |x: f32| table.sample_named("c", x).expect("curve");
        assert!(close(y(-1.0), before), "{mode:?} first - 1: {}", y(-1.0));
        assert!(close(y(21.0), after), "{mode:?} last + 1: {}", y(21.0));
        assert!(close(y(65.0), at_225), "{mode:?} last + 2.25 spans: {}", y(65.0));
        assert!(close(y(70.0), at_25), "{mode:?} last + 2.5 spans: {}", y(70.0));
    }
}

#[test]
fn linear_extrapolation_follows_the_end_tangent_of_a_cubic_curve_and_is_flat_for_a_step() {
    let mut points = keys(&[(0.0, 0.0), (1.0, 1.0)]);
    points[0].out_tangent = Some(3.0);
    points[1].in_tangent = Some(-2.0);
    let mut curve = NamedCurve::new("c", points);
    curve.pre = Extrap::Linear;
    curve.post = Extrap::Linear;
    let table = CurveTable::new(TableInterp::Cubic, vec![curve.clone()]).expect("table");
    assert!(close(table.sample_named("c", -1.0).expect("curve"), -3.0));
    assert!(close(table.sample_named("c", 2.0).expect("curve"), -1.0));
    let step = CurveTable::new(TableInterp::Step, vec![curve]).expect("table");
    assert_eq!(step.sample_named("c", -1.0), Some(0.0));
    assert_eq!(step.sample_named("c", 2.0), Some(1.0));
}

// Test 3: one key, rejected inputs.

#[test]
fn a_one_key_curve_is_constant_in_every_mode() {
    for mode in [
        Extrap::Hold,
        Extrap::Linear,
        Extrap::Cycle,
        Extrap::CycleOffset,
        Extrap::PingPong,
    ] {
        let mut curve = NamedCurve::new("c", keys(&[(3.0, 7.0)]));
        curve.pre = mode;
        curve.post = mode;
        let table = CurveTable::new(TableInterp::Cubic, vec![curve]).expect("table");
        for x in [-100.0, 3.0, 4.0, 1e6] {
            assert_eq!(table.sample_named("c", x), Some(7.0), "{mode:?} at {x}");
        }
    }
}

#[test]
fn bad_curves_are_rejected_with_their_codes() {
    let empty = CurveTable::new(TableInterp::Linear, vec![NamedCurve::new("e", vec![])]).expect_err("empty");
    assert_eq!(codes(&empty), ["curve_empty"]);
    assert_eq!(empty.issues[0].curve.as_deref(), Some("e"));

    let duplicate_x =
        CurveTable::from_ron_str(r#"(format_version: 1, curves: { "d": (keys: [(0.0, 1.0), (0.0, 2.0)]) })"#)
            .expect_err("duplicate x");
    assert_eq!(codes(&duplicate_x), ["curve_duplicate_x"]);

    let unsorted =
        CurveTable::from_ron_str(r#"(format_version: 1, curves: { "u": (keys: [(1.0, 1.0), (0.0, 2.0)]) })"#)
            .expect_err("unsorted x");
    assert_eq!(codes(&unsorted), ["curve_unsorted_x"]);

    let non_finite = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new("n", keys(&[(0.0, f32::NAN)]))],
    )
    .expect_err("nan");
    assert_eq!(codes(&non_finite), ["curve_bad_number"]);
    let infinite = CurveTable::from_ron_str(r#"(format_version: 1, curves: { "i": (keys: [(inf, 1.0)]) })"#)
        .expect_err("inf");
    assert_eq!(codes(&infinite), ["curve_bad_number"]);

    let names = CurveTable::new(
        TableInterp::Linear,
        vec![
            NamedCurve::new("a", keys(&[(0.0, 0.0)])),
            NamedCurve::new("a", keys(&[(0.0, 0.0)])),
            NamedCurve::new("bad name", keys(&[(0.0, 0.0)])),
        ],
    )
    .expect_err("names");
    assert_eq!(codes(&names), ["curve_duplicate_name", "curve_bad_name"]);

    let too_many = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new(
            "t",
            (0..=MAX_KEYS).map(|i| CurveKey::new(i as f32, 0.0)).collect(),
        )],
    )
    .expect_err("too many keys");
    assert_eq!(codes(&too_many), ["curve_too_many_keys"]);
}

#[test]
fn a_refused_edit_changes_nothing() {
    let mut table = one(TableInterp::Linear, &[(0.0, 0.0), (1.0, 1.0)]);
    let before = table.clone();
    let err = table.set_key("c", CurveKey::new(0.5, f32::NAN)).expect_err("nan");
    assert_eq!(codes(&err), ["curve_bad_number"]);
    let err = table.remove_key("c", 7.0).expect_err("no key");
    assert_eq!(codes(&err), ["key_not_found"]);
    let err = table
        .set_key("nope", CurveKey::new(0.5, 1.0))
        .expect_err("no curve");
    assert_eq!(codes(&err), ["curve_not_found"]);
    table.remove_key("c", 1.0).expect("remove");
    let err = table.remove_key("c", 0.0).expect_err("last key");
    assert_eq!(codes(&err), ["curve_empty"]);
    assert_ne!(table, before);
    table.rename_curve("c", "renamed").expect("rename");
    assert!(table.curve("renamed").is_some());
    table.delete_curve("renamed").expect("delete");
    assert!(table.is_empty());
}

// Test 4: CSV.

const CSV: &str = "name,0,10,30\r\nfalloff,1,0.6,0\r\nramp,0,,5\r\n";

#[test]
fn csv_reads_the_header_axis_and_blank_cells() {
    let table = CurveTable::from_csv_str(CSV, TableInterp::Linear).expect("csv");
    assert_eq!(table.len(), 2);
    let ramp = table.curve("ramp").expect("ramp");
    assert_eq!(ramp.keys, keys(&[(0.0, 0.0), (30.0, 5.0)]));
    assert_eq!(table.sample_named("falloff", 20.0), Some(0.3));
    assert_eq!(table.interp(), TableInterp::Linear);
}

#[test]
fn csv_accepts_bom_lf_and_quoted_fields() {
    let text = "\u{feff}name,\"0\",1\n\"a\",1,2\n";
    let table = CurveTable::from_csv_str(text, TableInterp::Step).expect("csv");
    assert_eq!(table.curve("a").expect("a").keys, keys(&[(0.0, 1.0), (1.0, 2.0)]));
    assert_eq!(table.interp(), TableInterp::Step);
}

#[test]
fn csv_errors_carry_codes_and_lines() {
    let ragged = CurveTable::from_csv_str("name,0,0\r\na,1,2\r\n", TableInterp::Linear).expect_err("repeat");
    assert_eq!(codes(&ragged), ["curve_csv_ragged"]);
    let not_number =
        CurveTable::from_csv_str("name,0,x\r\na,1,2\r\n", TableInterp::Linear).expect_err("axis");
    assert_eq!(codes(&not_number), ["curve_csv_ragged"]);
    let unsorted = CurveTable::from_csv_str("name,1,0\r\na,1,2\r\n", TableInterp::Linear).expect_err("order");
    assert_eq!(codes(&unsorted), ["curve_unsorted_x"]);
    let bad_cell =
        CurveTable::from_csv_str("name,0,1\r\na,1,oops\r\n", TableInterp::Linear).expect_err("cell");
    assert_eq!(codes(&bad_cell), ["curve_bad_number"]);
    assert_eq!(bad_cell.issues[0].line, Some(2));
    let empty_row =
        CurveTable::from_csv_str("name,0,1\r\na,1,2\r\nb,,\r\n", TableInterp::Linear).expect_err("empty");
    assert_eq!(codes(&empty_row), ["curve_empty"]);
    assert_eq!(empty_row.issues[0].line, Some(3));
    let unclosed = CurveTable::from_csv_str("name,0\r\n\"a,1\r\n", TableInterp::Linear).expect_err("quote");
    assert_eq!(codes(&unclosed), ["curve_bad_file"]);
    let header = CurveTable::from_csv_str("curve,0\r\na,1\r\n", TableInterp::Linear).expect_err("header");
    assert_eq!(codes(&header), ["curve_bad_file"]);
}

#[test]
fn ragged_axes_export_as_the_union_and_round_trip() {
    let table = CurveTable::new(
        TableInterp::Linear,
        vec![
            NamedCurve::new("a", keys(&[(0.0, 1.0), (2.5, 2.0)])),
            NamedCurve::new("b", keys(&[(1.0, 3.0), (2.5, 4.0), (8.0, 0.125)])),
        ],
    )
    .expect("table");
    let text = table.to_csv_string().expect("csv");
    assert_eq!(text, "name,0,1,2.5,8\r\na,1,,2,\r\nb,,3,4,0.125\r\n");
    let back = CurveTable::from_csv_str(&text, TableInterp::Linear).expect("re-read");
    assert_eq!(back, table);
    let again =
        CurveTable::from_csv_str(&back.to_csv_string().expect("csv"), TableInterp::Linear).expect("third");
    assert_eq!(again, back);
}

// Test 5: RON.

const RON: &str = r#"(
    format_version: 1,
    interp: Linear,                         // table default: Step | Linear | Cubic
    curves: {
        "falloff": (pre: Hold, post: Hold, keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
        "xp": (interp: Cubic, post: Linear, keys: [(1, 0), (2, 100), (x: 3, y: 250, out_tangent: 160.0)]),
    },
)"#;

#[test]
fn ron_reads_the_spec_example_in_file_order() {
    let table = CurveTable::from_ron_str(RON).expect("ron");
    let names: Vec<&str> = table.curves().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["falloff", "xp"]);
    let xp = table.curve("xp").expect("xp");
    assert_eq!(xp.interp, Some(TableInterp::Cubic));
    assert_eq!(xp.post, Extrap::Linear);
    assert_eq!(xp.keys[2].out_tangent, Some(160.0));
    assert_eq!(table.sample_named("xp", 2.0), Some(100.0));
    // Linear post extrapolation continues the explicit end tangent... of the last key's in side,
    // which is derived (the out tangent only shapes a segment starting at the last key).
    let past = table.sample_named("xp", 4.0).expect("xp");
    assert!(past > 250.0, "{past}");
}

#[test]
fn ron_round_trip_keeps_tangents_and_per_key_interpolation_byte_for_byte() {
    let mut curve = NamedCurve::new("c", keys(&[(0.0, 0.0), (0.1, 0.3), (2.0, 1.0)]));
    curve.keys[1].interp = Some(TableInterp::Step);
    curve.keys[1].in_tangent = Some(-0.5);
    curve.keys[2].out_tangent = Some(1e-7);
    curve.pre = Extrap::PingPong;
    curve.interp = Some(TableInterp::Cubic);
    let table = CurveTable::new(
        TableInterp::Step,
        vec![curve, NamedCurve::new("z.y-x_1", keys(&[(5.0, 6.0)]))],
    )
    .expect("table");
    let text = table.to_ron_string();
    let back = CurveTable::from_ron_str(&text).expect("re-read");
    assert_eq!(back, table);
    assert_eq!(back.to_ron_string(), text);
}

#[test]
fn ron_refuses_newer_and_zero_versions_and_unknown_fields() {
    let newer = CurveTable::from_ron_str(r#"(format_version: 2, curves: {})"#).expect_err("v2");
    assert_eq!(codes(&newer), ["curve_unsupported_version"]);
    let zero = CurveTable::from_ron_str(r#"(format_version: 0, curves: {})"#).expect_err("v0");
    assert_eq!(codes(&zero), ["curve_unsupported_version"]);
    let unknown = CurveTable::from_ron_str(r#"(format_version: 1, curvez: {})"#).expect_err("typo");
    assert_eq!(codes(&unknown), ["curve_bad_file"]);
    // The body is read first and the version peeked only on failure: a newer file whose shape
    // changed still reports its version, and text that is not RON still reports the syntax.
    let newer_shape = CurveTable::from_ron_str(r#"(format_version: 3, tracks: [])"#).expect_err("v3 shape");
    assert_eq!(codes(&newer_shape), ["curve_unsupported_version"]);
    let only_version =
        CurveTable::from_ron_str(r#"(version: 1, curves: {})"#).expect_err("no format_version");
    assert_eq!(codes(&only_version), ["curve_bad_file"]);
    let not_ron = CurveTable::from_ron_str("(format_version: 1, curves: {").expect_err("unclosed");
    assert_eq!(codes(&not_ron), ["curve_bad_file"]);
    let bad_key =
        CurveTable::from_ron_str(r#"(format_version: 1, curves: { "a": (keys: [(1.0, 2.0, 3.0)]) })"#)
            .expect_err("three numbers");
    assert_eq!(codes(&bad_key), ["curve_bad_file"]);
    assert!(curve_table_migration_sources().is_empty());
    assert!(curve_table_schema_fingerprint().contains("format_version"));
}

// Test 6: CurveRef.

#[test]
fn curve_refs_resolve_and_report_unresolved() {
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let good = CurveRef {
        table: "tables/falloff.curves.ron".into(),
        name: "falloff".into(),
    };
    assert!(good.resolve(&tables).is_ok());
    assert_eq!(good.sample(&tables, 5.0), Some(0.8));
    let missing_curve = CurveRef {
        name: "nope".into(),
        ..good.clone()
    };
    assert_eq!(
        missing_curve.resolve(&tables).expect_err("curve").code,
        "curve_ref_unresolved"
    );
    let missing_table = CurveRef {
        table: "tables/none.curves.ron".into(),
        ..good
    };
    assert_eq!(
        missing_table.resolve(&tables).expect_err("table").code,
        "curve_ref_unresolved"
    );
}

mod weapon_row {
    use regia_app::prelude::*;
    use serde::{Deserialize, Serialize};

    /// A data-table row holding a curve reference in two cells.
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    pub(super) struct Weapon {
        pub(super) damage: f32,
        pub(super) falloff_table: String,
        pub(super) falloff_curve: String,
    }
}
use weapon_row::Weapon;

#[test]
fn a_curve_ref_is_a_data_table_cell() {
    let rows = r#"(format_version: 1, row_type: "Weapon", rows: {
        "rifle": (damage: 30.0, falloff_table: "tables/falloff.curves.ron", falloff_curve: "falloff"),
        "pistol": (damage: 12.0, falloff_table: "tables/falloff.curves.ron", falloff_curve: "missing"),
    })"#;
    let weapons = regia_asset::data_table::DataTable::<Weapon>::from_ron_str(rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let resolve = |key: &str| {
        let row = weapons.get(key).expect("row");
        CurveRef {
            table: row.falloff_table.clone(),
            name: row.falloff_curve.clone(),
        }
        .resolve(&tables)
    };
    assert!(resolve("rifle").is_ok());
    assert_eq!(
        resolve("pistol").expect_err("missing").code,
        "curve_ref_unresolved"
    );
}

// Spec 51 § 2.1: a typed row can hold a `CurveRef` as a field (it derives `Reflect`), and the
// validator pass reports each row whose reference does not resolve.

mod curve_ref_row {
    use regia_app::prelude::*;
    use serde::{Deserialize, Serialize};

    /// A data-table row holding a curve reference as one field.
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    pub(super) struct Weapon {
        pub(super) damage: f32,
        pub(super) falloff: super::CurveRef,
    }
}
use curve_ref_row::Weapon as CurveRefWeapon;

#[test]
fn a_curve_ref_field_of_a_reflected_row_resolves() {
    let rows = r#"(format_version: 1, row_type: "Weapon", rows: {
        "rifle": (damage: 30.0, falloff: (table: "tables/falloff.curves.ron", name: "falloff")),
    })"#;
    let weapons = regia_asset::data_table::DataTable::<CurveRefWeapon>::from_ron_str(rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    assert!(
        weapons
            .get("rifle")
            .expect("row")
            .falloff
            .resolve(&tables)
            .is_ok()
    );
}

#[test]
fn the_validator_pass_reports_each_unresolved_curve_ref_row() {
    let rows = r#"(format_version: 1, row_type: "Weapon", rows: {
        "rifle": (damage: 30.0, falloff: (table: "tables/falloff.curves.ron", name: "falloff")),
        "pistol": (damage: 12.0, falloff: (table: "tables/falloff.curves.ron", name: "missing")),
        "bow": (damage: 40.0, falloff: (table: "tables/none.curves.ron", name: "falloff")),
    })"#;
    let weapons = regia_asset::data_table::DataTable::<CurveRefWeapon>::from_ron_str(rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let issues = validate_curve_refs(&weapons, &tables);
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        ["curve_ref_unresolved", "curve_ref_unresolved"],
        "{issues:?}"
    );
    assert!(
        issues
            .iter()
            .all(|i| i.message.contains("pistol") || i.message.contains("bow")),
        "each issue names its row: {issues:?}"
    );
    assert!(
        issues.iter().all(|i| i.is_error()),
        "unresolved references are errors: {issues:?}"
    );
    // Every reference resolving is clean.
    let good = r#"(format_version: 1, row_type: "Weapon", rows: {
        "rifle": (damage: 30.0, falloff: (table: "tables/falloff.curves.ron", name: "falloff")),
    })"#;
    let weapons = regia_asset::data_table::DataTable::<CurveRefWeapon>::from_ron_str(good).expect("table");
    assert!(validate_curve_refs(&weapons, &tables).is_empty());
}

#[test]
fn the_validator_pass_walks_nested_structs_and_options() {
    use regia_app::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    struct Nested {
        note: String,
        inner: CurveRef,
        maybe: Option<CurveRef>,
    }
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    struct Row {
        top: CurveRef,
        nested: Nested,
    }
    let row = |key: &str, name: &str| {
        format!(
            r#""{key}": (top: (table: "tables/falloff.curves.ron", name: "falloff"), nested: (note: "n", inner: (table: "tables/falloff.curves.ron", name: "{name}"), maybe: Some((table: "tables/falloff.curves.ron", name: "falloff"))))"#
        )
    };
    let rows = format!(
        "(format_version: 1, row_type: \"Row\", rows: {{ {}, {} }} )",
        row("good", "falloff"),
        row("bad", "missing")
    );
    let table = regia_asset::data_table::DataTable::<Row>::from_ron_str(&rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let issues = validate_curve_refs(&table, &tables);
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        ["curve_ref_unresolved"],
        "one bad nested reference: {issues:?}"
    );
    assert!(
        issues[0].message.contains("bad"),
        "names the row: {:?}",
        issues[0]
    );
}

#[test]
fn the_validator_pass_walks_lists_tuples_maps_and_enum_variants() {
    use regia_app::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    struct Falloff(CurveRef, f32);
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    enum Damage {
        Curve(CurveRef),
        Flat(f32),
    }
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    struct Row {
        list: Vec<CurveRef>,
        pair: (CurveRef, f32),
        tuple_struct: Falloff,
        map: std::collections::HashMap<String, CurveRef>,
        damage: Damage,
    }
    let rows = r#"(format_version: 1, row_type: "Row", rows: {
        "bad": (
            list: [(table: "tables/falloff.curves.ron", name: "missing")],
            pair: ((table: "tables/falloff.curves.ron", name: "missing"), 1.0),
            tuple_struct: ((table: "tables/falloff.curves.ron", name: "missing"), 1.0),
            map: {"a": (table: "tables/falloff.curves.ron", name: "missing")},
            damage: Curve((table: "tables/falloff.curves.ron", name: "missing")),
        ),
        "flat": (
            list: [],
            pair: ((table: "tables/falloff.curves.ron", name: "falloff"), 2.0),
            tuple_struct: ((table: "tables/falloff.curves.ron", name: "falloff"), 1.0),
            map: {},
            damage: Flat(3.0),
        ),
    })"#;
    let table = regia_asset::data_table::DataTable::<Row>::from_ron_str(rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let issues = validate_curve_refs(&table, &tables);
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        ["curve_ref_unresolved"; 5],
        "one issue per dangling reference in list/pair/tuple-struct/map/enum: {issues:?}"
    );
    assert!(
        issues.iter().all(|i| i.message.contains("bad")),
        "each issue names its row: {issues:?}"
    );
}

// Spec 51 § 8.4: the curve CSV reader conforms to the same RFC 4180 rules as the data-table
// conformance fixtures (docs/specs/content-data.md § 3.3).

#[test]
fn csv_line_endings_and_a_byte_order_mark_read_the_same_curves() {
    let lf = "name,0,10\nfalloff,1,0.6\n";
    let want = CurveTable::from_csv_str(lf, TableInterp::Linear).expect("LF");
    for text in [
        lf.replace('\n', "\r\n"),
        lf.replace('\n', "\r"),
        format!("\u{feff}{lf}"),
        format!("\u{feff}{}", lf.replace('\n', "\r\n").trim_end()),
    ] {
        let table = CurveTable::from_csv_str(&text, TableInterp::Linear).expect("variant loads");
        assert_eq!(table, want, "{text:?}");
    }
}

#[test]
fn quoted_fields_and_names_read_like_plain_ones() {
    // RFC 4180 quoting is accepted on any field; the curve grammar limits names to
    // [A-Za-z0-9_.-] and cells to finite numbers, so nothing a valid file holds needs quoting,
    // and a written CSV is never quoted.
    let plain = "name,0,10\nfalloff,1,0.6\nz.y-x_1,0,5\n";
    let quoted = "\"name\",\"0\",\"10\"\r\n\"falloff\",\"1\",\"0.6\"\r\n\"z.y-x_1\",\"0\",\"5\"\r\n";
    let want = CurveTable::from_csv_str(plain, TableInterp::Linear).expect("plain");
    assert_eq!(
        CurveTable::from_csv_str(quoted, TableInterp::Linear).expect("quoted"),
        want
    );
    // The writer emits plain fields and CRLF records; the round trip is byte-identical.
    let text = want.to_csv_string().expect("csv");
    assert_eq!(text, plain.replace('\n', "\r\n"));
    assert_eq!(
        CurveTable::from_csv_str(&text, TableInterp::Linear).expect("re-read"),
        want
    );
}

// Test 19 (curve half): a bad reload leaves the previous table active.

#[test]
fn a_bad_reload_keeps_the_previous_table_and_reports_the_error() {
    let mut tables = CurveTables::default();
    let path = "tables/falloff.curves.ron";
    assert_eq!(tables.load_text(path, RON), Ok(1));
    let issues = tables
        .load_text(path, r#"(format_version: 1, curves: { "a": (keys: []) })"#)
        .expect_err("empty curve");
    assert_eq!(issues[0].code, "curve_empty");
    assert_eq!(tables.generation(path), 1);
    assert_eq!(
        tables.get(path).expect("table").sample_named("falloff", 5.0),
        Some(0.8)
    );
    let slot = tables.slot(path).expect("slot");
    assert_eq!(slot.rejected_reloads, 1);
    assert_eq!(slot.last_issues[0].code, "curve_empty");
    assert_eq!(tables.load_text(path, RON), Ok(2), "a good reload applies");
    assert_eq!(tables.slot(path).expect("slot").reloads, 1);
    assert_eq!(
        tables.load_text("tables/falloff.curves.csv", CSV),
        Ok(1),
        "`.csv` paths read CSV"
    );
}

// Audit fixes (ADR-1809).

#[test]
fn a_reload_that_reorders_curves_makes_old_handles_stale() {
    let mut tables = CurveTables::default();
    let path = "tables/ab.curves.csv";
    tables
        .load_text(path, "name,0,1\na,1,1\nb,2,2\n")
        .expect("first load");
    let b = tables.get(path).and_then(|t| t.handle("b")).expect("handle b");
    assert_eq!(tables.get(path).and_then(|t| t.try_sample(b, 0.5)), Some(2.0));
    tables
        .load_text(path, "name,0,1\nb,2,2\na,1,1\n")
        .expect("reordered reload");
    let table = tables.get(path).expect("table");
    assert_eq!(
        table.try_sample(b, 0.5),
        None,
        "the old handle of `b` must not sample `a` at the same index"
    );
    let fresh = table.handle("b").expect("new handle");
    assert_eq!(table.try_sample(fresh, 0.5), Some(2.0));
}

#[test]
fn a_reload_that_only_changes_keys_keeps_handles() {
    let mut tables = CurveTables::default();
    let path = "tables/ab.curves.csv";
    tables
        .load_text(path, "name,0,1\na,1,1\nb,2,2\n")
        .expect("first load");
    let b = tables.get(path).and_then(|t| t.handle("b")).expect("handle b");
    tables
        .load_text(path, "name,0,1\na,1,1\nb,3,3\n")
        .expect("key-only reload");
    assert_eq!(tables.get(path).and_then(|t| t.try_sample(b, 0.5)), Some(3.0));
}

#[test]
fn a_handle_is_never_valid_in_another_table_of_the_same_shape() {
    let first = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new("a", keys(&[(0.0, 1.0)]))],
    )
    .expect("first");
    let second = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new("b", keys(&[(0.0, 2.0)]))],
    )
    .expect("second");
    let handle = first.handle("a").expect("handle");
    assert!(first.is_valid(handle));
    assert!(!second.is_valid(handle));
    assert_eq!(second.try_sample(handle, 0.0), None);
}

#[test]
fn a_failed_step_of_a_multi_step_edit_leaves_the_table_and_generation_unchanged() {
    let mut tables = CurveTables::default();
    let path = "tables/falloff.curves.ron";
    assert_eq!(tables.load_text(path, RON), Ok(1));
    let error = tables
        .edit(path, |t| {
            let mut curve = t.curve("falloff").cloned().expect("curve");
            curve.keys = keys(&[(0.0, 5.0)]);
            t.upsert_curve(curve)?;
            t.rename_curve("falloff", "bad name")
        })
        .expect_err("bad name");
    assert_eq!(codes(&error), ["curve_bad_name"]);
    assert_eq!(tables.generation(path), 1);
    let table = tables.get(path).expect("table");
    assert_eq!(
        table.sample_named("falloff", 10.0),
        Some(0.6),
        "the key edit was rolled back"
    );
    assert!(table.curve("bad name").is_none());
}

#[test]
fn far_cycle_extrapolation_folds_in_f64() {
    // x = 50331652 = 3 * 16777217 + 1: loop 16777217 (odd), 1 into it. An f32 quotient rounds to
    // 16777218 (even, local -2 clamped to 0) and read the wrong half at the wrong place.
    let table = |mode: Extrap| {
        let mut curve = NamedCurve::new("c", keys(&[(0.0, 0.0), (3.0, 3.0)]));
        curve.post = mode;
        CurveTable::new(TableInterp::Linear, vec![curve]).expect("table")
    };
    let x = 50_331_652.0_f32;
    assert_eq!(
        table(Extrap::PingPong).sample_named("c", x),
        Some(2.0),
        "odd loop mirrors"
    );
    assert_eq!(table(Extrap::Cycle).sample_named("c", x), Some(1.0));
}

#[test]
fn far_cycle_offset_accumulates_loops_in_f64() {
    // Loop 16777217 is not an f32; the offset must still add a whole y-span per loop.
    let mut curve = NamedCurve::new("c", keys(&[(0.0, 0.0), (3.0, 3.0)]));
    curve.post = Extrap::CycleOffset;
    let table = CurveTable::new(TableInterp::Linear, vec![curve]).expect("table");
    assert_eq!(table.sample_named("c", 50_331_652.0_f32), Some(50_331_652.0));
}

// The cooked (pak) form, spec 51 § 4.4 (ADR-2012).

fn cooked_fixture() -> CurveTable {
    let mut tangents = CurveKey::new(1.0, 2.0);
    tangents.interp = Some(TableInterp::Cubic);
    tangents.in_tangent = Some(0.5);
    tangents.out_tangent = Some(-0.25);
    let mut with_extrap = NamedCurve::new("b.c-d_e", vec![CurveKey::new(0.0, 1.0), tangents]);
    with_extrap.pre = Extrap::PingPong;
    with_extrap.post = Extrap::CycleOffset;
    with_extrap.interp = Some(TableInterp::Step);
    CurveTable::new(
        TableInterp::Cubic,
        vec![
            NamedCurve::new("a", keys(&[(0.0, 0.0), (1.0, 1.0), (3.0, 1.5)])),
            with_extrap,
        ],
    )
    .expect("valid table")
}

/// The version prefix of a cooked payload, replaced by `version`.
fn with_cooked_version(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut out = version.to_le_bytes().to_vec();
    out.extend_from_slice(&bytes[4..]);
    out
}

#[test]
fn a_cooked_table_is_the_table_it_was_cooked_from() {
    let table = cooked_fixture();
    let cooked = table.to_cooked().expect("cook");
    assert_eq!(
        cooked[..4],
        CURVE_TABLE_COOKED_VERSION.to_le_bytes(),
        "a little-endian version comes first"
    );
    let loaded = CurveTable::from_cooked(&cooked).expect("load");
    assert_eq!(loaded, table);
    // Same text, so every per-key, per-curve and table setting survived; same samples, so the
    // derived tangents were baked again on load.
    assert_eq!(loaded.to_ron_string(), table.to_ron_string());
    for x in [-3.5, -0.25, 0.0, 0.5, 1.0, 2.0, 3.0, 4.75, 40.0] {
        for curve in ["a", "b.c-d_e"] {
            assert_eq!(
                loaded.sample_named(curve, x).map(f32::to_bits),
                table.sample_named(curve, x).map(f32::to_bits),
                "{curve} at {x}"
            );
        }
    }
}

#[test]
fn cooking_is_stable_for_the_same_table() {
    let table = cooked_fixture();
    let first = table.to_cooked().expect("cook");
    let again = CurveTable::from_cooked(&first)
        .expect("load")
        .to_cooked()
        .expect("cook");
    assert_eq!(first, again, "a cache key may rest on these bytes");
}

#[test]
fn an_unknown_cooked_version_is_refused_by_name() {
    let cooked = cooked_fixture().to_cooked().expect("cook");
    for version in [0, CURVE_TABLE_COOKED_VERSION + 1, u32::MAX] {
        let error = CurveTable::from_cooked(&with_cooked_version(&cooked, version)).expect_err("refused");
        assert_eq!(codes(&error), ["curve_unsupported_version"], "version {version}");
    }
}

#[test]
fn every_truncation_of_a_cooked_table_is_an_error_and_never_a_panic() {
    let cooked = cooked_fixture().to_cooked().expect("cook");
    for len in 0..cooked.len() {
        let error = CurveTable::from_cooked(&cooked[..len]).expect_err("truncated");
        assert_eq!(codes(&error), ["curve_bad_file"], "length {len}");
    }
}

#[test]
fn trailing_bytes_after_a_cooked_table_are_refused() {
    let mut cooked = cooked_fixture().to_cooked().expect("cook");
    cooked.push(0);
    let error = CurveTable::from_cooked(&cooked).expect_err("trailing byte");
    assert_eq!(codes(&error), ["curve_bad_file"]);
}

/// A cooked payload is not trusted: it may come from a damaged or edited pak, so what the text
/// path refuses stays refused.
#[test]
fn a_cooked_table_that_breaks_a_rule_is_refused_by_the_rule_s_own_code() {
    let cooked = |curves: Vec<NamedCurve>| cooked_payload(TableInterp::Linear, &curves).expect("encode");
    let cases = [
        ("curve_empty", vec![NamedCurve::new("a", Vec::new())]),
        (
            "curve_bad_number",
            vec![NamedCurve::new("a", keys(&[(0.0, f32::NAN)]))],
        ),
        (
            "curve_unsorted_x",
            vec![NamedCurve::new("a", keys(&[(1.0, 0.0), (0.0, 0.0)]))],
        ),
        (
            "curve_duplicate_x",
            vec![NamedCurve::new("a", keys(&[(1.0, 0.0), (1.0, 0.0)]))],
        ),
        (
            "curve_bad_name",
            vec![NamedCurve::new("a b", keys(&[(0.0, 0.0)]))],
        ),
        (
            "curve_duplicate_name",
            vec![
                NamedCurve::new("a", keys(&[(0.0, 0.0)])),
                NamedCurve::new("a", keys(&[(0.0, 0.0)])),
            ],
        ),
    ];
    for (code, curves) in cases {
        let error = CurveTable::from_cooked(&cooked(curves)).expect_err(code);
        assert!(codes(&error).contains(&code), "{code}: {error}");
    }
}

#[test]
fn a_length_prefix_far_past_the_end_fails_without_reserving_it() {
    // Version 1, interp 0, then a curve count of about 2^63 with nothing behind it.
    let mut bytes = CURVE_TABLE_COOKED_VERSION.to_le_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
    let error = CurveTable::from_cooked(&bytes).expect_err("no such curves");
    assert_eq!(codes(&error), ["curve_bad_file"]);
}

#[test]
fn the_cooked_load_goes_through_the_slot_and_says_where_it_came_from() {
    let mut tables = CurveTables::default();
    assert!(!tables.slot("t.curves.ron").is_some_and(|s| s.cooked));
    let cooked = cooked_fixture().to_cooked().expect("cook");
    let generation = tables.load_cooked("t.curves.ron", &cooked).expect("load");
    assert_eq!(generation, 1);
    assert!(tables.slot("t.curves.ron").expect("slot").cooked);
    assert_eq!(tables.get("t.curves.ron"), Some(&cooked_fixture()));
    // Text afterwards (a hot reload) is no longer the cooked table.
    let text = cooked_fixture().to_ron_string();
    tables.load_text("t.curves.ron", &text).expect("reload");
    assert!(!tables.slot("t.curves.ron").expect("slot").cooked);
    // A bad payload keeps the table in use and reports why.
    let generation = tables.generation("t.curves.ron");
    let issues = tables
        .load_cooked("t.curves.ron", b"\x01\0\0\0junk")
        .expect_err("refused");
    assert_eq!(issues[0].code, "curve_bad_file");
    assert_eq!(tables.generation("t.curves.ron"), generation);
    assert_eq!(tables.get("t.curves.ron"), Some(&cooked_fixture()));
    assert_eq!(
        tables.slot("t.curves.ron").expect("slot").last_issues[0].code,
        "curve_bad_file"
    );
}

// Spec-conformance lane (spec 51 audit, 2026-11-10): limits of § 2.1 that had no test.

/// § 2.1: names are 1 to 64 bytes of `[A-Za-z0-9_.-]`; a table holds at most 4,096 curves.
#[test]
fn name_length_and_curve_count_limits() {
    let name64 = "a".repeat(MAX_CURVE_NAME_LEN);
    let name65 = "a".repeat(MAX_CURVE_NAME_LEN + 1);
    assert!(
        CurveTable::new(
            TableInterp::Linear,
            vec![NamedCurve::new(&name64, keys(&[(0.0, 1.0)]))]
        )
        .is_ok()
    );
    let long = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new(&name65, keys(&[(0.0, 1.0)]))],
    )
    .expect_err("65 bytes");
    assert_eq!(codes(&long), ["curve_bad_name"]);
    for ok in ["A.b-c_9", "x"] {
        assert!(is_valid_curve_name(ok), "{ok}");
    }
    for bad in ["", "a b", "é", "a/b"] {
        assert!(!is_valid_curve_name(bad), "{bad:?}");
    }
    let curves = |n: usize| -> Vec<NamedCurve> {
        (0..n)
            .map(|i| NamedCurve::new(&format!("c{i}"), keys(&[(0.0, 1.0)])))
            .collect()
    };
    let full = CurveTable::new(TableInterp::Linear, curves(MAX_CURVES)).expect("4,096 curves load");
    assert_eq!(full.len(), MAX_CURVES);
    let over = CurveTable::new(TableInterp::Linear, curves(MAX_CURVES + 1)).expect_err("4,097 curves");
    assert_eq!(codes(&over), ["curve_too_many_curves"]);
}

/// § 2.1: `curve_sample_nonfinite` is reported once per second of frame time per curve, not per
/// sample: the counts stay until a second of frame time has passed, then one report drains them.
#[test]
fn non_finite_samples_are_reported_at_most_once_per_second() {
    use regia_app::prelude::{Time, World};
    use std::time::Duration;
    let mut world = World::new();
    let mut tables = CurveTables::default();
    tables.insert(
        "t/c.curves.ron",
        one(TableInterp::Linear, &[(0.0, 1.0), (1.0, 2.0)]),
    );
    world.insert_resource(tables);
    world.insert_resource(Time::default());
    let sample_nan = |world: &World| {
        let tables = world.resource::<CurveTables>();
        let table = tables.get("t/c.curves.ron").expect("table");
        assert_eq!(table.sample_named("c", f32::NAN), Some(1.0));
    };
    // One registered system, so its `Local` frame-time accumulator persists between runs.
    let system = world.register_system(warn_nonfinite_samples);
    let frame = |world: &mut World| {
        world.resource_mut::<Time>().advance(Duration::from_millis(250));
        world.run_system(system).expect("system runs");
    };
    sample_nan(&world);
    sample_nan(&world);
    for _ in 0..3 {
        frame(&mut world);
    }
    // 0.75 s of frame time: nothing reported yet, both samples still counted.
    let pending = |world: &World| {
        world
            .resource::<CurveTables>()
            .get("t/c.curves.ron")
            .expect("table")
            .take_nonfinite_counts()
    };
    assert_eq!(pending(&world), [("c".to_owned(), 2)]);
    // `take` drained them; count again, then the fourth frame (1.0 s) reports and drains.
    sample_nan(&world);
    frame(&mut world);
    assert!(pending(&world).is_empty(), "the one-second report took the count");
}

mod weapon_ref_row {
    use regia_app::prelude::*;
    use serde::{Deserialize, Serialize};

    /// A data-table row holding a `CurveRef` field (spec 51 § 2.1, test 6).
    #[derive(Serialize, Deserialize, Reflect, Debug, Clone, PartialEq)]
    pub(super) struct WeaponRef {
        pub(super) damage: f32,
        pub(super) falloff: super::CurveRef,
    }
}

/// Spec 51 § 8 test 6: a `CurveRef` field in a typed data-table row resolves, and a missing curve
/// reports `curve_ref_unresolved`.
#[test]
fn a_curve_ref_field_in_a_data_table_row_resolves() {
    use weapon_ref_row::WeaponRef;
    let rows = r#"(format_version: 1, row_type: "WeaponRef", rows: {
        "rifle": (damage: 30.0, falloff: (table: "tables/falloff.curves.ron", name: "falloff")),
        "pistol": (damage: 12.0, falloff: (table: "tables/falloff.curves.ron", name: "missing")),
    })"#;
    let weapons = regia_asset::data_table::DataTable::<WeaponRef>::from_ron_str(rows).expect("table");
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/falloff.curves.ron",
        CurveTable::from_ron_str(RON).expect("ron"),
    );
    let rifle = &weapons.get("rifle").expect("rifle").falloff;
    assert!(rifle.resolve(&tables).is_ok());
    assert_eq!(rifle.sample(&tables, 5.0), Some(0.8));
    let pistol = &weapons.get("pistol").expect("pistol").falloff;
    assert_eq!(
        pistol.resolve(&tables).expect_err("missing").code,
        "curve_ref_unresolved"
    );
}
