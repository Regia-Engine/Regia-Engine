// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 11-04 (ADR-2705): edge cases of the value reader/writer, attribute columns, point sets,
//! seeds, run limits and the node-output cache of `regia_procgen`. Deterministic: fixed inputs, no clocks.

use regia_core::id::Guid;
use regia_procgen::ProcNodeRegistry;
use regia_procgen::data::{
    AttrColumn, AttrType, AttrValue, Bundle, Point, PointSet, Xform, is_reserved_name, table_from_ron,
};
use regia_procgen::exec::{CancelToken, ProcCache, RunLimits, RunLimitsOverride};
use regia_procgen::graph::{
    Edge, NodeEntry, ProcGraph, pgraph_migration_sources, pgraph_schema_fingerprint, pgvar_migration_sources,
    pgvar_schema_fingerprint, valid_param_name,
};
use regia_procgen::polygon::{BoolOp, Join, PolygonData, PolygonError, ring_area};
use regia_procgen::seeds::{
    cell_key, draw, element_rng, gen_seed_from_guid, node_seed, point_seed, q, tier_seed,
};
use regia_procgen::tiles::{AnchorSettings, TileSettings, TileState, quality_reach_index};
use regia_procgen::value::SettingValue;
use regia_render::quality::QualityTier;

// ---- SettingValue ----

#[test]
fn numbers_keep_their_kind_through_write_and_parse() {
    for text in ["0", "-7", "9007199254740993", "1.5", "-0.25", "1e-9", "0.0"] {
        let v = SettingValue::parse(text).expect("parses");
        let again = SettingValue::parse(&v.write()).expect("reparses");
        assert_eq!(v, again, "{text}");
    }
    assert_eq!(SettingValue::parse("3"), Ok(SettingValue::Int(3)));
    assert_eq!(SettingValue::parse("3.0"), Ok(SettingValue::Float(3.0)));
    // A whole float is still written as a float so it does not read back as an integer.
    assert_eq!(SettingValue::Float(2.0).write(), "2.0");
    assert_eq!(SettingValue::Int(2).write(), "2");
}

#[test]
fn strings_escape_and_round_trip() {
    let nasty = "a \"quoted\" \\ back\nnew\ttab é";
    let v = SettingValue::Str(nasty.to_string());
    let text = v.write();
    assert_eq!(SettingValue::parse(&text), Ok(v));
}

#[test]
fn nested_values_write_canonically() {
    let text = "Weighted(entries: [(mesh: \"a.glb\", weight: 3.0)], flag: true, none: None)";
    let v = SettingValue::parse(text).expect("parses");
    let once = v.write();
    let twice = SettingValue::parse(&once).expect("reparses").write();
    assert_eq!(once, twice, "write is a fixed point");
    match &v {
        SettingValue::Record(Some(name), fields) => {
            assert_eq!(name, "Weighted");
            assert_eq!(fields.len(), 3);
        }
        other => panic!("expected a named record, got {other:?}"),
    }
    assert_eq!(v.field("flag"), Some(&SettingValue::Bool(true)));
    assert_eq!(v.field("missing"), None);
    assert_eq!(SettingValue::Int(1).field("flag"), None);
}

#[test]
fn malformed_input_reports_an_offset_instead_of_panicking() {
    for bad in [
        "",
        "(",
        "[1, 2",
        "\"unterminated",
        "(a: )",
        "1 2",
        "{1: }",
        ")",
        "]",
        "(1,, 2)",
        "@",
    ] {
        assert!(SettingValue::parse(bad).is_err(), "{bad:?} should not parse");
    }
    let err = SettingValue::parse("[1, 2").expect_err("must fail");
    assert!(err.offset <= 5);
    assert!(err.to_string().contains("RON parse error"));
    let deep = "(".repeat(5000) + &")".repeat(5000);
    assert!(SettingValue::parse(&deep).is_err(), "depth is bounded");
}

#[test]
fn accessors_convert_only_what_is_exact() {
    assert_eq!(SettingValue::Int(4).as_f64(), Some(4.0));
    assert_eq!(SettingValue::Float(4.0).as_i64(), Some(4));
    assert_eq!(SettingValue::Float(4.5).as_i64(), None);
    assert_eq!(
        SettingValue::Float(1.0e16).as_i64(),
        None,
        "beyond exact f64 integers"
    );
    assert_eq!(SettingValue::Str("x".into()).as_f64(), None);
    assert_eq!(SettingValue::Ident("Foo".into()).as_text(), Some("Foo"));
    assert_eq!(SettingValue::Bool(true).as_text(), None);
    assert_eq!(
        SettingValue::Float(2.5).as_vec3(),
        Some([2.5; 3]),
        "a scalar is a uniform vector"
    );
    let t = SettingValue::parse("(1, 2.5, -3)").expect("parses");
    assert_eq!(t.as_vec3(), Some([1.0, 2.5, -3.0]));
    assert_eq!(SettingValue::parse("(1, 2)").expect("parses").as_vec3(), None);
    assert_eq!(
        SettingValue::parse("(1, 2, \"x\")").expect("parses").as_vec3(),
        None
    );
    assert_eq!(
        SettingValue::parse("[1, 2]")
            .expect("parses")
            .as_items()
            .map(<[_]>::len),
        Some(2)
    );
}

#[test]
fn binds_resolve_inside_every_container_and_report_the_unknown() {
    let v = SettingValue::parse("(a: Bind(\"x\"), b: [Bind(\"y\"), Bind(\"nope\")], c: Thing(Bind(\"x\")))")
        .expect("parses");
    let mut unknown = Vec::new();
    let lookup = |name: &str| match name {
        "x" => Some(SettingValue::Int(1)),
        "y" => Some(SettingValue::Float(2.5)),
        _ => None,
    };
    let out = v.resolve_binds(&lookup, &mut unknown);
    assert_eq!(unknown, vec!["nope".to_string()]);
    assert_eq!(out.field("a"), Some(&SettingValue::Int(1)));
    let b = out.field("b").and_then(SettingValue::as_items).expect("list");
    assert_eq!(b[0], SettingValue::Float(2.5));
    assert_eq!(b[1].as_bind(), Some("nope"), "an unknown bind stays in place");
    assert_eq!(
        out.field("c"),
        Some(&SettingValue::Call("Thing".into(), vec![SettingValue::Int(1)]))
    );
    assert_eq!(
        SettingValue::Call("Bind".into(), vec![SettingValue::Int(1)]).as_bind(),
        None
    );
}

// ---- attribute types and columns ----

#[test]
fn attribute_type_names_round_trip_and_widths_are_fixed() {
    for ty in AttrType::ALL {
        assert_eq!(AttrType::from_name(ty.name()), Some(ty));
    }
    assert_eq!(AttrType::from_name("float"), None, "names are case sensitive");
    let widths: Vec<_> = AttrType::ALL.iter().map(|t| t.width()).collect();
    assert_eq!(
        widths,
        [
            Some(1),
            Some(1),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(4),
            None,
            None,
            None
        ]
    );
}

#[test]
fn column_push_refuses_a_value_of_another_type() {
    let mut c = AttrColumn::new(AttrType::Float);
    assert!(c.is_empty());
    assert!(c.push(AttrValue::Float(1.5)));
    assert!(!c.push(AttrValue::Int(2)), "no silent conversion");
    assert_eq!(c.len(), 1);
    assert_eq!(c.get(0), Some(AttrValue::Float(1.5)));
    assert_eq!(c.get(1), None, "out of range is None, not a panic");
}

#[test]
fn filled_columns_use_the_documented_defaults() {
    let q = AttrColumn::filled(AttrType::Quat, 2);
    assert_eq!(
        q.get(1),
        Some(AttrValue::Quat([0.0, 0.0, 0.0, 1.0])),
        "identity rotation"
    );
    let t = AttrColumn::filled(AttrType::Transform, 1);
    assert_eq!(t.get(0), Some(AttrValue::Transform(Xform::default())));
    assert_eq!(Xform::default().s, [1.0; 3]);
    for ty in AttrType::ALL {
        let c = AttrColumn::filled(ty, 3);
        assert_eq!(c.ty(), ty);
        assert_eq!(c.len(), 3);
    }
}

#[test]
fn column_extend_pads_missing_and_refuses_a_type_clash() {
    let mut c = AttrColumn::Int(vec![1].into());
    assert!(c.extend_from(None, 2));
    assert_eq!(c, AttrColumn::Int(vec![1, 0, 0].into()));
    assert!(c.extend_from(Some(&AttrColumn::Int(vec![9].into())), 1));
    assert_eq!(c.len(), 4);
    assert!(!c.extend_from(Some(&AttrColumn::Float(vec![1.0].into())), 1));
    assert_eq!(c.len(), 4, "a refused extend changes nothing");
}

#[test]
fn column_select_reorders_and_repeats() {
    let c = AttrColumn::Name(vec!["a".into(), "b".into(), "c".into()].into());
    assert_eq!(
        c.select(&[2, 0, 0]),
        AttrColumn::Name(vec!["c".into(), "a".into(), "a".into()].into())
    );
    assert!(c.select(&[]).is_empty());
}

#[test]
fn values_expose_components_and_key_text() {
    assert_eq!(AttrValue::Bool(true).components(), Some(vec![1.0]));
    assert_eq!(
        AttrValue::Vec3([1.0, 2.0, 3.0]).components(),
        Some(vec![1.0, 2.0, 3.0])
    );
    assert_eq!(AttrValue::Name("x".into()).components(), None);
    assert_eq!(AttrValue::Int(-5).key_text(), "-5");
    assert_eq!(AttrValue::Bool(false).key_text(), "false");
    assert_eq!(AttrValue::Asset("m.glb".into()).key_text(), "m.glb");
    assert_eq!(AttrValue::Vec2([1.0, 2.0]).key_text(), "[1.0, 2.0]");
    assert_eq!(AttrValue::Float(1.0).as_key_text(), None);
    assert_eq!(AttrValue::Transform(Xform::default()).key_text(), "");
}

#[test]
fn non_finite_values_are_located_per_column() {
    assert_eq!(
        AttrColumn::Float(vec![0.0, f64::NAN, 1.0].into()).first_non_finite(),
        Some(1)
    );
    assert_eq!(
        AttrColumn::Vec3(vec![[0.0; 3], [0.0, f64::INFINITY, 0.0]].into()).first_non_finite(),
        Some(1)
    );
    let mut bad = Xform::default();
    bad.s[2] = f64::NEG_INFINITY;
    assert_eq!(
        AttrColumn::Transform(vec![Xform::default(), bad].into()).first_non_finite(),
        Some(1)
    );
    assert_eq!(AttrColumn::Int(vec![i64::MAX].into()).first_non_finite(), None);
    assert_eq!(
        AttrColumn::Name(vec!["nan".into()].into()).first_non_finite(),
        None
    );
}

#[test]
fn reserved_names_are_the_builtins_and_anything_underscored() {
    assert!(is_reserved_name("position") && is_reserved_name("hardness") && is_reserved_name("_tmp"));
    assert!(!is_reserved_name("kind") && !is_reserved_name("Position"));
}

// ---- point sets ----

fn pt(x: f32, seed: u32) -> Point {
    Point {
        position: [x, 0.0, 0.0],
        seed,
        ..Point::default()
    }
}

#[test]
fn push_clamps_density_and_hardness_and_pads_attributes() {
    let mut s = PointSet::new();
    s.push(pt(0.0, 1));
    s.attrs.insert("kind".into(), AttrColumn::Int(vec![7].into()));
    s.push(Point {
        density: 3.0,
        hardness: -2.0,
        ..pt(1.0, 2)
    });
    assert_eq!(s.len(), 2);
    assert_eq!((s.density[1], s.hardness[1]), (1.0, 0.0));
    assert_eq!(
        s.attrs["kind"],
        AttrColumn::Int(vec![7, 0].into()),
        "a new point gets the default value"
    );
    s.set_point(
        0,
        Point {
            density: -1.0,
            ..pt(5.0, 9)
        },
    );
    assert_eq!(s.point(0).density, 0.0);
    assert_eq!(s.point(0).seed, 9);
}

#[test]
fn partition_splits_without_losing_or_reordering() {
    let mut s = PointSet::new();
    for i in 0..5 {
        s.push(pt(i as f32, i));
    }
    s.attrs.insert("n".into(), AttrColumn::Int((0..5).collect()));
    let (kept, dropped) = s.partition(&[true, false, true, false, false]);
    assert_eq!(kept.seed, vec![0, 2]);
    assert_eq!(dropped.seed, vec![1, 3, 4]);
    assert_eq!(kept.attrs["n"], AttrColumn::Int(vec![0, 2].into()));
    assert_eq!(dropped.attrs["n"], AttrColumn::Int(vec![1, 3, 4].into()));
    // A mask shorter than the set sends the unmasked points nowhere: the contract is per masked index.
    let (a, b) = s.partition(&[]);
    assert!(a.is_empty() && b.is_empty());
}

#[test]
fn append_aligns_attribute_columns_and_refuses_clashes() {
    let mut a = PointSet::new();
    a.push(pt(0.0, 1));
    a.attrs
        .insert("only_a".into(), AttrColumn::Float(vec![2.5].into()));
    let mut b = PointSet::new();
    b.push(pt(1.0, 2));
    b.push(pt(2.0, 3));
    b.attrs
        .insert("only_b".into(), AttrColumn::Bool(vec![true, true].into()));
    a.append(&b).expect("compatible");
    assert_eq!(a.len(), 3);
    assert_eq!(a.attrs["only_a"], AttrColumn::Float(vec![2.5, 0.0, 0.0].into()));
    assert_eq!(
        a.attrs["only_b"],
        AttrColumn::Bool(vec![false, true, true].into())
    );
    for col in a.attrs.values() {
        assert_eq!(col.len(), a.len());
    }
    let mut clash = PointSet::new();
    clash.push(pt(0.0, 0));
    clash
        .attrs
        .insert("only_a".into(), AttrColumn::Int(vec![1].into()));
    let before = a.clone();
    assert_eq!(a.append(&clash), Err("only_a".to_string()));
    assert_eq!(a, before, "a refused append changes nothing");
}

#[test]
fn contains_falls_off_with_hardness_and_stops_at_the_box() {
    let mut s = PointSet::new();
    s.push(Point {
        density: 0.8,
        hardness: 1.0,
        ..Point::default()
    });
    s.push(Point {
        density: 1.0,
        hardness: 0.5,
        ..Point::default()
    });
    assert_eq!(
        s.contains(0, [0.4, 0.0, 0.0]),
        0.8,
        "hard edge keeps full density"
    );
    assert_eq!(s.contains(0, [0.6, 0.0, 0.0]), 0.0, "outside the box");
    assert_eq!(s.contains(1, [0.0, 0.0, 0.0]), 1.0);
    assert_eq!(s.contains(1, [0.25, 0.0, 0.0]), 1.0, "inside the hard core");
    let mid = s.contains(1, [0.375, 0.0, 0.0]);
    assert!(
        (mid - 0.5).abs() < 1e-5,
        "halfway between core and surface, got {mid}"
    );
    assert_eq!(
        s.contains(1, [0.5, 0.0, 0.0]),
        0.0,
        "the surface itself has fallen to zero"
    );
}

#[test]
fn contains_follows_rotation_and_scale() {
    let mut s = PointSet::new();
    // A 4 m long box along local x, turned 90 degrees about y: it now spans z.
    let half_turn = std::f32::consts::FRAC_PI_4;
    s.push(Point {
        rotation: [0.0, half_turn.sin(), 0.0, half_turn.cos()],
        scale: [4.0, 1.0, 1.0],
        ..Point::default()
    });
    assert!(s.contains(0, [0.0, 0.0, 1.5]) > 0.0);
    assert_eq!(s.contains(0, [1.5, 0.0, 0.0]), 0.0);
    let (lo, hi) = s.world_aabb(0);
    assert!((hi[2] - lo[2] - 4.0).abs() < 1e-4, "z extent {lo:?} {hi:?}");
    assert!((hi[0] - lo[0] - 1.0).abs() < 1e-4, "x extent {lo:?} {hi:?}");
}

#[test]
fn a_degenerate_rotation_acts_as_identity() {
    let mut s = PointSet::new();
    s.push(Point {
        rotation: [0.0; 4],
        ..Point::default()
    });
    s.push(Point {
        rotation: [f32::NAN, 0.0, 0.0, 1.0],
        ..Point::default()
    });
    for i in 0..2 {
        let (lo, hi) = s.world_aabb(i);
        assert_eq!((lo, hi), ([-0.5; 3], [0.5; 3]), "point {i}");
    }
}

#[test]
fn first_non_finite_names_the_offending_stream() {
    let mut s = PointSet::new();
    s.push(pt(0.0, 0));
    assert_eq!(s.first_non_finite(), None);
    s.push(Point {
        color: [0.0, f32::NAN, 0.0, 1.0],
        ..pt(1.0, 1)
    });
    assert_eq!(s.first_non_finite(), Some(("color".to_string(), 1)));
    let mut s = PointSet::new();
    s.push(pt(0.0, 0));
    s.attrs
        .insert("w".into(), AttrColumn::Float(vec![f64::INFINITY].into()));
    assert_eq!(s.first_non_finite(), Some(("w".to_string(), 0)));
}

// ---- seeds ----

#[test]
fn seed_derivation_is_a_pure_function_with_independent_levels() {
    let g = gen_seed_from_guid(Guid(0x1234_5678_9abc_def0_1111_2222_3333_4444));
    assert_eq!(
        g,
        gen_seed_from_guid(Guid(0x1234_5678_9abc_def0_9999_9999_9999_9999))
    );
    assert_ne!(tier_seed(g, 0), tier_seed(g, 1));
    assert_ne!(node_seed(tier_seed(g, 0), 1), node_seed(tier_seed(g, 0), 2));
    assert_ne!(cell_key(1, 2), cell_key(2, 1), "cell keys are not symmetric");
    assert_eq!(cell_key(-1, -1), cell_key(-1, -1));
}

#[test]
fn point_seed_quantizes_to_millimeters_and_tolerates_extremes() {
    assert_eq!(q(0.0004), 0);
    assert_eq!(q(0.0006), 1);
    assert_eq!(q(-0.0006), u32::MAX, "negative values wrap through i32");
    assert_eq!(
        point_seed([f32::MAX, f32::MIN, 0.0], 1),
        point_seed([f32::MAX, f32::MIN, 0.0], 1)
    );
    // NaN quantizes to zero rather than panicking.
    assert_eq!(q(f32::NAN), 0);
    assert_eq!(
        point_seed([f32::NAN, 0.0, 0.0], 3),
        point_seed([0.0, 0.0, 0.0], 3)
    );
}

#[test]
fn draws_are_uniform_enough_and_independent_across_slots() {
    let mut sum = 0.0;
    let n = 4000;
    for e in 0..n {
        let d = draw(e, 77, 0);
        assert!((0.0..1.0).contains(&d));
        sum += d;
    }
    let mean = sum / f64::from(n);
    assert!((mean - 0.5).abs() < 0.03, "mean {mean}");
    let mut a = element_rng(5, 6, 0);
    let mut b = element_rng(5, 6, 0);
    assert_eq!(a.f64(), b.f64(), "same stream, same values");
    assert_ne!(draw(5, 6, 0), draw(5, 6, 1));
}

// ---- run limits and the cache ----

#[test]
fn limit_overrides_can_only_lower_never_raise() {
    let base = RunLimits::DESKTOP;
    let lowered = base.with_override(&RunLimitsOverride {
        points_per_item: Some(10),
        call_depth: Some(4),
        ..RunLimitsOverride::default()
    });
    assert_eq!(lowered.points_per_item, 10);
    assert_eq!(lowered.call_depth, 4);
    assert_eq!(
        lowered.total_elements, base.total_elements,
        "unset fields keep the base"
    );
    let raised = base.with_override(&RunLimitsOverride {
        points_per_item: Some(usize::MAX),
        memory_bytes: Some(usize::MAX),
        ..RunLimitsOverride::default()
    });
    assert_eq!(raised, base, "an override above the platform maximum is ignored");
    const { assert!(RunLimits::ANDROID.memory_bytes < RunLimits::DESKTOP.memory_bytes) };
    const { assert!(RunLimits::ANDROID.points_per_item <= RunLimits::DESKTOP.points_per_item) };
}

#[test]
fn cancel_tokens_share_state_across_clones() {
    let a = CancelToken::new();
    let b = a.clone();
    assert!(!b.is_cancelled());
    a.cancel();
    assert!(b.is_cancelled());
    a.cancel();
    assert!(a.is_cancelled(), "cancelling twice is harmless");
}

#[test]
fn an_empty_cache_reports_zeroes_and_empty_bundles_cost_nothing() {
    let c = ProcCache::new(1024);
    assert_eq!(c.stats(), (0, 0));
    assert_eq!(c.used_bytes(), 0);
    assert_eq!(regia_procgen::exec::estimate_bytes(&Bundle::empty()), 0);
}

#[test]
fn point_bundle_size_estimate_grows_with_points_and_attributes() {
    use regia_procgen::data::{DataItem, Payload};
    let mut small = PointSet::new();
    small.push(pt(0.0, 0));
    let mut big = small.clone();
    for i in 1..10 {
        big.push(pt(i as f32, i));
    }
    let with_attr = {
        let mut s = big.clone();
        s.attrs
            .insert("v".into(), AttrColumn::filled(AttrType::Vec3, s.len()));
        s
    };
    let est =
        |p: PointSet| regia_procgen::exec::estimate_bytes(&Bundle(vec![DataItem::new(Payload::Points(p))]));
    let (a, b, c) = (est(small), est(big), est(with_attr));
    assert!(a > 0 && b > a && c > b, "{a} {b} {c}");
    assert_eq!(b, a * 10, "per-point cost is linear");
}

// ---- polygons ----

fn square(x0: f64, z0: f64, x1: f64, z1: f64) -> PolygonData {
    PolygonData::rect([x0, z0, x1, z1], 0.0)
}

#[test]
fn edge_distance_is_the_distance_to_the_nearest_edge_inside_and_out() {
    let sq = square(0.0, 0.0, 10.0, 10.0);
    assert!((sq.edge_distance(5.0, 5.0) - 5.0).abs() < 1e-9, "center");
    assert!(
        (sq.edge_distance(1.0, 5.0) - 1.0).abs() < 1e-9,
        "inside near the left edge"
    );
    assert!((sq.edge_distance(-3.0, 5.0) - 3.0).abs() < 1e-9, "outside");
    assert!(
        (sq.edge_distance(13.0, 14.0) - 5.0).abs() < 1e-9,
        "outside a corner: 3-4-5"
    );
    assert_eq!(sq.edge_distance(0.0, 5.0), 0.0, "on the boundary");
    assert!(
        PolygonData::default().edge_distance(0.0, 0.0).is_infinite(),
        "no rings, no edges"
    );
}

#[test]
fn edge_distance_follows_the_polygon_origin() {
    let moved = PolygonData::rect([100.0, 200.0, 110.0, 210.0], 5.0);
    assert!((moved.edge_distance(105.0, 205.0) - 5.0).abs() < 1e-9);
    assert!((moved.edge_distance(95.0, 205.0) - 5.0).abs() < 1e-9);
    assert_eq!(moved.bounds(), Some([100.0, 200.0, 110.0, 210.0]));
    assert!(moved.contains(101.0, 201.0) && !moved.contains(99.0, 201.0));
}

#[test]
fn union_all_merges_overlaps_keeps_disjoint_pieces_and_handles_no_input() {
    assert_eq!(PolygonData::union_all(&[]), Ok(None));
    let a = square(0.0, 0.0, 10.0, 10.0);
    let b = square(5.0, 0.0, 15.0, 10.0);
    let c = square(100.0, 100.0, 110.0, 110.0);
    let two = PolygonData::union_all(&[&a, &b]).expect("ok").expect("some");
    assert!(
        (two.area() - 150.0).abs() < 1e-6,
        "overlap counted once, got {}",
        two.area()
    );
    assert_eq!(two.pieces().len(), 1);
    let three = PolygonData::union_all(&[&a, &b, &c]).expect("ok").expect("some");
    assert!((three.area() - 250.0).abs() < 1e-6);
    assert_eq!(three.pieces().len(), 2, "the far square is its own piece");
    // The result is in the first polygon's frame.
    assert_eq!(three.origin, a.origin);
    assert!(three.contains(105.0, 105.0));
}

#[test]
fn union_all_reports_a_polygon_beyond_the_grid() {
    let a = square(0.0, 0.0, 1.0, 1.0);
    let far = square(5.0e6, 0.0, 5.0e6 + 1.0, 1.0);
    assert_eq!(PolygonData::union_all(&[&a, &far]), Err(PolygonError::Range));
    assert_eq!(PolygonError::Range.code(), "polygon_range");
    assert!(PolygonError::Range.to_string().contains("beyond"));
}

#[test]
fn booleans_obey_their_set_identities() {
    let a = square(0.0, 0.0, 10.0, 10.0);
    let b = square(5.0, 5.0, 15.0, 15.0);
    let area = |op| a.boolean(&b, op).expect("ok").area();
    let (u, i, d, x) = (
        area(BoolOp::Union),
        area(BoolOp::Intersection),
        area(BoolOp::Difference),
        area(BoolOp::Xor),
    );
    assert!(
        (u - 175.0).abs() < 1e-6
            && (i - 25.0).abs() < 1e-6
            && (d - 75.0).abs() < 1e-6
            && (x - 150.0).abs() < 1e-6
    );
    assert!(
        (u - (a.area() + b.area() - i)).abs() < 1e-6,
        "inclusion-exclusion"
    );
    assert!((x - (u - i)).abs() < 1e-6);
    let none = a
        .boolean(&square(50.0, 50.0, 60.0, 60.0), BoolOp::Intersection)
        .expect("ok");
    assert!(none.is_empty(), "disjoint intersection has no rings");
    assert!(
        a.boolean(&a, BoolOp::Difference).expect("ok").is_empty(),
        "a minus itself"
    );
}

#[test]
fn operation_and_join_names_parse() {
    assert_eq!(BoolOp::from_name("union"), Some(BoolOp::Union));
    assert_eq!(BoolOp::from_name("intersection"), Some(BoolOp::Intersection));
    assert_eq!(BoolOp::from_name("difference"), Some(BoolOp::Difference));
    assert_eq!(BoolOp::from_name("xor"), Some(BoolOp::Xor));
    assert_eq!(BoolOp::from_name("Union"), None);
    assert_eq!(Join::from_name("round"), Join::Round);
    assert_eq!(Join::from_name("square"), Join::Square);
    assert_eq!(Join::from_name("miter"), Join::Miter);
    assert_eq!(Join::from_name("???"), Join::Miter, "anything else is a miter");
}

#[test]
fn a_hole_ring_subtracts_and_the_even_odd_rule_cuts_self_overlap() {
    let outer = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
    let hole = vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]];
    let p = PolygonData::new([0.0; 3], &[outer, hole]).expect("ok");
    assert!((p.area() - 84.0).abs() < 1e-6, "100 - 16, got {}", p.area());
    assert!(p.contains(1.0, 1.0) && !p.contains(5.0, 5.0));
    assert!(
        (p.edge_distance(5.0, 5.0) - 2.0).abs() < 1e-9,
        "from the hole's center to the hole edge"
    );
    assert!(
        ring_area(&[[0.0, 0.0], [1.0, 0.0]]) == 0.0,
        "fewer than 3 vertices has no area"
    );
    assert!(
        ring_area(&[[0.0, 0.0], [0.0, 1.0], [1.0, 0.0]]) < 0.0,
        "clockwise is negative"
    );
}

#[test]
fn centroid_of_a_rectangle_is_its_middle_and_offset_changes_the_area() {
    let r = square(10.0, 20.0, 14.0, 30.0);
    let c = r.centroid();
    assert!((c[0] - 12.0).abs() < 1e-9 && (c[1] - 25.0).abs() < 1e-9, "{c:?}");
    let bigger = r.offset(1.0, Join::Miter, 2.0).expect("ok");
    assert!((bigger.area() - 6.0 * 12.0).abs() < 1e-3, "{}", bigger.area());
    let smaller = r.offset(-1.0, Join::Miter, 2.0).expect("ok");
    assert!((smaller.area() - 2.0 * 8.0).abs() < 1e-3, "{}", smaller.area());
    let gone = r.offset(-3.0, Join::Miter, 2.0).expect("ok");
    assert!(gone.is_empty(), "shrinking past the half width leaves nothing");
}

// ---- table files, tile states, anchor settings ----

#[test]
fn a_ron_table_infers_a_type_per_column() {
    let t = table_from_ron(
        "(rows: { \"a\": (n: 1, w: 2.5, f: true, name: \"x\", v: (1.0, 2.0)), \"b\": (n: 2, w: 3, f: false, name: \"y\", v: (3.0, 4.0)) })",
    )
    .expect("parses");
    assert_eq!(t.rows, 2);
    assert_eq!(t.columns[0].0, "key");
    assert_eq!(t.column("n").map(AttrColumn::ty), Some(AttrType::Int));
    assert_eq!(
        t.column("w").map(AttrColumn::ty),
        Some(AttrType::Float),
        "an int mixed with a float widens"
    );
    assert_eq!(t.column("f").map(AttrColumn::ty), Some(AttrType::Bool));
    assert_eq!(t.column("name").map(AttrColumn::ty), Some(AttrType::Name));
    assert_eq!(t.column("v").map(AttrColumn::ty), Some(AttrType::Vec2));
    assert_eq!(t.column("w").and_then(|c| c.get(1)), Some(AttrValue::Float(3.0)));
    assert_eq!(
        t.column("key").and_then(|c| c.get(0)),
        Some(AttrValue::Name("a".into()))
    );
}

#[test]
fn a_ron_table_with_gaps_fills_defaults_and_mixed_columns_are_refused() {
    let t = table_from_ron("(rows: { \"a\": (n: 5), \"b\": (m: 1.5) })").expect("parses");
    assert_eq!(
        t.column("n").and_then(|c| c.get(1)),
        Some(AttrValue::Int(0)),
        "a missing value is zero"
    );
    assert_eq!(t.column("m").and_then(|c| c.get(0)), Some(AttrValue::Float(0.0)));
    let err = table_from_ron("(rows: { \"a\": (x: 1), \"b\": (x: \"s\") })").expect_err("must fail");
    assert!(err.starts_with("table_column_types"), "{err}");
    for bad in ["", "(", "7", "(other: 1)", "(rows: 3)", "(rows: { \"a\": 3 })"] {
        let err = table_from_ron(bad).expect_err("must fail");
        assert!(err.starts_with("table_parse"), "{bad:?} -> {err}");
    }
    let empty = table_from_ron("(rows: {})").expect("parses");
    assert_eq!(empty.rows, 0);
}

#[test]
fn a_v0_list_table_still_reads() {
    let t = table_from_ron("[(\"k1\", (a: 1)), (\"k2\", (a: 2))]").expect("parses");
    assert_eq!(t.rows, 2);
    assert_eq!(t.column("a").map(AttrColumn::ty), Some(AttrType::Int));
    assert!(
        table_from_ron("[1, 2]")
            .expect_err("must fail")
            .starts_with("table_parse")
    );
}

#[test]
fn tile_state_names_round_trip_and_in_flight_states_are_the_early_ones() {
    let all = [
        TileState::Queued,
        TileState::Capturing,
        TileState::Running,
        TileState::Applying,
        TileState::Resident,
        TileState::Releasing,
        TileState::Failed,
    ];
    for s in all {
        assert_eq!(TileState::from_name(s.name()), Some(s));
    }
    assert_eq!(TileState::from_name("RESIDENT"), None);
    assert_eq!(TileState::from_name(""), None);
}

#[test]
fn anchor_defaults_and_the_quality_scale_table() {
    let a = AnchorSettings::default();
    assert_eq!(a.base_reach_m, 256.0);
    assert!(a.max_new() >= 1 && a.max_running() >= 1);
    assert!(a.apply_ms() > 0.0 && a.resident_mb() >= 128);
    // The renderer tiers map onto four reach slots, low to ultra.
    assert_eq!(quality_reach_index(QualityTier::Low), 0);
    assert_eq!(quality_reach_index(QualityTier::MobileLow), 0);
    assert_eq!(quality_reach_index(QualityTier::Medium), 1);
    assert_eq!(quality_reach_index(QualityTier::High), 2);
    assert_eq!(quality_reach_index(QualityTier::Ultra), 3);
    assert!(a.quality_scale(QualityTier::Low) < a.quality_scale(QualityTier::Ultra));
    let custom = AnchorSettings {
        max_new_per_frame: Some(3),
        max_running: Some(9),
        ..AnchorSettings::default()
    };
    assert_eq!((custom.max_new(), custom.max_running()), (3, 9));
}

#[test]
fn reach_for_finds_a_declared_tier_by_cell_size_and_falls_back_to_the_base_reach() {
    use regia_procgen::tiles::TierReach;
    let a = AnchorSettings {
        tiers: vec![TierReach {
            cell_m: 64.0,
            build_m: 100.0,
            ..TierReach::default()
        }],
        ..AnchorSettings::default()
    };
    assert_eq!(a.reach_for(Some(64.0)).build_m, 100.0);
    let fallback = a.reach_for(Some(512.0));
    assert_eq!((fallback.cell_m, fallback.build_m), (512.0, a.base_reach_m));
    assert_eq!(a.reach_for(None).cell_m, 0.0, "the whole tier has no cell size");
    let tight = TierReach {
        release_factor: 0.2,
        ..TierReach::default()
    };
    assert_eq!(tight.release_factor(), 1.1, "release must exceed build");
    assert_eq!(TileSettings::default().cell_m, 128.0);
}

// ---- graph files ----

#[test]
fn param_names_follow_the_documented_pattern() {
    for ok in ["a", "radius_m", "a1_b2", &"x".repeat(64)] {
        assert!(valid_param_name(ok), "{ok}");
    }
    for bad in ["", "A", "1a", "_a", "a-b", "a b", "é", &"x".repeat(65)] {
        assert!(!valid_param_name(bad), "{bad:?}");
    }
}

#[test]
fn with_ron_stores_unparsable_text_as_a_string() {
    let n = NodeEntry::new("sample.ground")
        .with_ron("good", "[1, 2]")
        .with_ron("bad", "[1, 2");
    assert_eq!(
        n.settings["good"],
        SettingValue::List(vec![SettingValue::Int(1), SettingValue::Int(2)])
    );
    assert_eq!(n.settings["bad"], SettingValue::Str("[1, 2".into()));
    assert!(n.enabled && n.seed_salt.is_none() && n.v.is_none());
}

#[test]
fn node_ids_continue_after_the_highest_and_saturate() {
    let mut g = ProcGraph::default();
    assert_eq!(g.next_node_id(), 1);
    g.nodes.insert(7, NodeEntry::new("in.self"));
    g.nodes.insert(3, NodeEntry::new("in.self"));
    assert_eq!(g.next_node_id(), 8);
    g.nodes.insert(u32::MAX, NodeEntry::new("in.self"));
    assert_eq!(g.next_node_id(), u32::MAX, "no wrap-around to an id in use");
    assert!(g.param("nothing").is_none());
}

#[test]
fn the_graph_hash_ignores_canvas_position_and_sees_settings_and_wiring() {
    let reg = ProcNodeRegistry::with_builtins();
    let base = |x: f64, density: &str| {
        let mut g = ProcGraph::default();
        let mut a = NodeEntry::new("in.self");
        a.at = (x, x);
        g.nodes.insert(1, a);
        g.nodes.insert(
            2,
            NodeEntry::new("sample.ground").with("spacing_m", SettingValue::parse(density).expect("parses")),
        );
        g.edges.push(Edge::new(1, "out", 2, "region"));
        g
    };
    let h = base(0.0, "4.0").graph_hash(&reg);
    assert_eq!(h, base(0.0, "4.0").graph_hash(&reg), "repeatable");
    assert_eq!(
        h,
        base(500.0, "4.0").graph_hash(&reg),
        "the canvas position is not content"
    );
    assert_ne!(h, base(0.0, "5.0").graph_hash(&reg), "a setting is content");
    let mut rewired = base(0.0, "4.0");
    rewired.edges.clear();
    assert_ne!(h, rewired.graph_hash(&reg), "wiring is content");
    let mut off = base(0.0, "4.0");
    if let Some(n) = off.nodes.get_mut(&2) {
        n.enabled = false;
    }
    assert_ne!(h, off.graph_hash(&reg), "bypassing a node changes the graph");
}

#[test]
fn graph_text_round_trips_and_garbage_is_a_coded_error() {
    let mut g = ProcGraph {
        description: "a \"quoted\" description".to_string(),
        ..ProcGraph::default()
    };
    g.nodes.insert(1, NodeEntry::new("in.self"));
    g.nodes.insert(
        2,
        NodeEntry::new("filter.range")
            .with_ron("attr", "\"density\"")
            .with_ron("min", "0.5"),
    );
    g.edges.push(Edge::new(1, "out", 2, "in"));
    let text = g.to_ron();
    let back = ProcGraph::from_ron(&text).expect("reads its own output");
    assert_eq!(back.to_ron(), text, "write is a fixed point");
    assert_eq!(back.nodes.len(), 2);
    assert_eq!(back.edges, g.edges);
    assert_eq!(back.description, g.description);
    for bad in ["", "(", "ProcGraph(", "7", "ProcGraph(format_version: \"x\")"] {
        let err = ProcGraph::from_ron(bad).expect_err("must fail");
        assert!(
            !err.code.is_empty() && err.code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn schema_fingerprints_name_every_attribute_type_and_have_no_migrations_yet() {
    let f = pgraph_schema_fingerprint();
    for ty in AttrType::ALL {
        assert!(
            f.contains(ty.name()),
            "{} missing from the pgraph fingerprint",
            ty.name()
        );
    }
    assert!(f.starts_with("ProcGraph(format_version"));
    assert_eq!(f, pgraph_schema_fingerprint());
    assert_ne!(f, pgvar_schema_fingerprint());
    assert!(pgvar_schema_fingerprint().contains("overrides"));
    assert!(pgraph_migration_sources().is_empty() && pgvar_migration_sources().is_empty());
}
