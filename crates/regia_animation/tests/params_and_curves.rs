// SPDX-License-Identifier: MPL-2.0
//! Edge-case coverage for the parameter set and the editable float curves: the paths the
//! in-module tests leave out (type namespaces, trigger bookkeeping, listing order, RON round
//! trips, degenerate curves, key editing at the boundaries, baking fallbacks).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use regia_animation::clip::TrackCurve;
use regia_animation::curve::{Curve, CurveKey, CurveKeyKind, hermite};
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose_eval::ParamLayout;
use regia_core::math::{Vec2, Vec3};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards every request to the system allocator and only counts the ones this thread
// can record. A thread whose local state is not available is not counted.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get().saturating_add(1)));
        // SAFETY: the layout is the caller's, as `GlobalAlloc::alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer came from this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get().saturating_add(1)));
        // SAFETY: the pointer came from this allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

fn speed_graph() -> AnimGraph {
    AnimGraph {
        parameters: vec![
            ParamDecl("Speed".into(), ParamValue::Float(0.0)),
            ParamDecl("Move".into(), ParamValue::Vec2(Vec2::ZERO)),
            ParamDecl("Crouch".into(), ParamValue::Bool(false)),
        ],
        root: AnimNode::Clip(ClipNode {
            clip: "idle".into(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        }),
    }
}

#[test]
fn param_value_conversions_pick_the_matching_variant() {
    assert_eq!(ParamValue::from(1.5_f32), ParamValue::Float(1.5));
    assert_eq!(ParamValue::from(2.0_f64), ParamValue::Float(2.0));
    assert_eq!(ParamValue::from(7_i32), ParamValue::Int(7));
    assert_eq!(ParamValue::from(true), ParamValue::Bool(true));
    assert_eq!(
        ParamValue::from(Vec2::new(1.0, 2.0)),
        ParamValue::Vec2(Vec2::new(1.0, 2.0))
    );
    assert_eq!(
        ParamValue::from(Vec3::new(1.0, 2.0, 3.0)),
        ParamValue::Vec3(Vec3::new(1.0, 2.0, 3.0))
    );
}

#[test]
fn same_name_in_different_types_does_not_collide() {
    let mut p = ParameterSet::new();
    p.set_float("X", 1.5);
    p.set_int("X", 4);
    p.set_bool("X", true);
    assert_eq!(p.float("X"), 1.5);
    assert_eq!(p.int("X"), 4);
    assert!(p.bool("X"));
    // `get` resolves a shared name in a fixed order: float first.
    assert_eq!(p.get("X"), Some(ParamValue::Float(1.5)));
    assert_eq!(p.get("Missing"), None);
}

#[test]
fn get_reads_a_trigger_as_a_bool_without_consuming_it() {
    let mut p = ParameterSet::from_decls(&[ParamDecl("Jump".into(), ParamValue::Trigger)]);
    assert_eq!(p.get("Jump"), Some(ParamValue::Bool(false)), "declared but unset");
    p.set_trigger("Jump");
    assert_eq!(p.get("Jump"), Some(ParamValue::Bool(true)));
    assert_eq!(
        p.get("Jump"),
        Some(ParamValue::Bool(true)),
        "reading does not consume"
    );
    assert!(p.consume_trigger("Jump"));
    assert_eq!(
        p.get("Jump"),
        Some(ParamValue::Bool(false)),
        "the declaration survives a consume"
    );
}

#[test]
fn trigger_fired_twice_is_consumed_once() {
    let mut p = ParameterSet::new();
    p.set_trigger("Hit");
    p.set_trigger("Hit");
    assert!(p.consume_trigger("Hit"));
    assert!(!p.consume_trigger("Hit"), "triggers are flags, not counters");
}

#[test]
fn consume_trigger_ignores_non_trigger_parameters() {
    let mut p = ParameterSet::new();
    p.set_bool("Crouch", true);
    assert!(!p.consume_trigger("Crouch"));
    assert!(p.bool("Crouch"), "the bool is untouched");
    assert!(!p.trigger("Crouch"));
}

#[test]
fn from_decls_covers_every_kind_and_the_last_duplicate_wins() {
    let decls = [
        ParamDecl("F".into(), ParamValue::Float(0.25)),
        ParamDecl("I".into(), ParamValue::Int(-3)),
        ParamDecl("B".into(), ParamValue::Bool(true)),
        ParamDecl("V2".into(), ParamValue::Vec2(Vec2::new(1.0, 2.0))),
        ParamDecl("V3".into(), ParamValue::Vec3(Vec3::new(3.0, 4.0, 5.0))),
        ParamDecl("T".into(), ParamValue::Trigger),
        ParamDecl("F".into(), ParamValue::Float(9.0)),
    ];
    let p = ParameterSet::from_decls(&decls);
    assert_eq!(p.float("F"), 9.0);
    assert_eq!(p.int("I"), -3);
    assert!(p.bool("B"));
    assert_eq!(p.vec2("V2"), Vec2::new(1.0, 2.0));
    assert_eq!(p.vec3("V3"), Vec3::new(3.0, 4.0, 5.0));
    assert!(!p.trigger("T"));
    assert_eq!(p.vec2("Nope"), Vec2::ZERO);
    assert_eq!(p.vec3("Nope"), Vec3::ZERO);
    assert_eq!(p.int("Nope"), 0);
}

#[test]
fn listings_are_sorted_and_split_triggers_from_values() {
    let mut p = ParameterSet::new();
    p.set_float("b_float", 1.0);
    p.set_int("a_int", 2);
    p.set_bool("c_bool", true);
    p.set_trigger("z_trig");
    p.set_trigger("d_trig");
    assert!(p.consume_trigger("d_trig"));

    let values = p.values();
    let names: Vec<&str> = values.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["a_int", "b_float", "c_bool"],
        "values() omits triggers and sorts by name"
    );

    assert_eq!(
        p.triggers(),
        vec![("d_trig".to_string(), false), ("z_trig".to_string(), true)]
    );

    let entries = p.entries();
    let entry_names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(entry_names, ["a_int", "b_float", "c_bool", "d_trig", "z_trig"]);
    assert_eq!(entries[3].1, ParamValue::Bool(false));
    assert_eq!(entries[4].1, ParamValue::Bool(true));
}

#[test]
fn empty_parameter_set_lists_nothing() {
    let p = ParameterSet::new();
    assert!(p.values().is_empty());
    assert!(p.triggers().is_empty());
    assert!(p.entries().is_empty());
    assert_eq!(p, ParameterSet::default());
}

#[test]
fn parameter_set_survives_a_ron_round_trip() {
    let mut p = ParameterSet::new();
    p.set_float("Speed", 3.5);
    p.set_int("Combo", 2);
    p.set_bool("Crouch", true);
    p.set_vec2("Move", Vec2::new(0.5, -0.5));
    p.set_vec3("Aim", Vec3::new(0.0, 1.0, -2.0));
    p.set_trigger("Jump");
    let text = ron::to_string(&p).expect("serialize");
    let back: ParameterSet = ron::from_str(&text).expect("deserialize");
    assert_eq!(back, p);
    assert!(back.trigger("Jump"), "a pending trigger stays pending");
}

#[test]
fn malformed_param_ron_is_an_error_not_a_panic() {
    assert!(ron::from_str::<ParamValue>("Float(").is_err());
    assert!(ron::from_str::<ParamValue>("Quaternion(1.0)").is_err());
    assert!(ron::from_str::<ParamDecl>("(\"Speed\")").is_err());
}

#[test]
fn hermite_hits_its_end_values() {
    assert!((hermite(2.0, 5.0, 10.0, -10.0, 0.0) - 2.0).abs() < 1e-6);
    assert!((hermite(2.0, 5.0, 10.0, -10.0, 1.0) - 5.0).abs() < 1e-6);
}

#[test]
fn cubic_from_angles_stores_the_tangent_of_each_angle() {
    match CurveKeyKind::cubic_from_angles(std::f32::consts::FRAC_PI_4, 0.0) {
        CurveKeyKind::Cubic {
            left_tangent,
            right_tangent,
        } => {
            assert!((left_tangent - 1.0).abs() < 1e-5);
            assert!(right_tangent.abs() < 1e-6);
        }
        other => panic!("expected a cubic key, got {other:?}"),
    }
}

#[test]
fn single_key_curve_is_constant_everywhere() {
    let curve = Curve::from(vec![CurveKey::new(2.0, 3.0, CurveKeyKind::Linear)]);
    assert!(!curve.is_empty());
    assert_eq!(curve.value_at(-10.0), 3.0);
    assert_eq!(curve.value_at(2.0), 3.0);
    assert_eq!(curve.value_at(10.0), 3.0);
    assert_eq!(curve.bounds(), [2.0, 3.0, 2.0, 3.0]);
    assert_eq!(curve.max_location(), 2.0);
    assert!(matches!(curve.bake(30.0, 1.0), TrackCurve::Static(v) if v == 3.0));
}

#[test]
fn empty_curve_reports_zeroes() {
    let curve = Curve::default();
    assert!(curve.is_empty());
    assert_eq!(curve.max_location(), 0.0);
    assert_eq!(curve.bounds(), [0.0; 4]);
    let mut editable = curve.clone();
    assert!(editable.remove_key(0).is_none());
    assert!(editable.move_key(0, 1.0, 1.0).is_none());
    assert!(editable.is_empty());
}

#[test]
fn move_key_with_a_bad_index_leaves_the_curve_unchanged() {
    let mut curve = Curve::from(vec![
        CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
        CurveKey::new(1.0, 1.0, CurveKeyKind::Linear),
    ]);
    let before = curve.clone();
    assert!(curve.move_key(5, 0.5, 9.0).is_none());
    assert_eq!(curve, before);
}

#[test]
fn add_key_at_an_existing_location_inserts_before_it() {
    let mut curve = Curve::from(vec![CurveKey::new(0.0, 1.0, CurveKeyKind::Linear)]);
    let at = curve.add_key(CurveKey::new(0.0, 2.0, CurveKeyKind::Linear));
    assert_eq!(at, 0);
    assert_eq!(curve.keys.len(), 2);
    assert_eq!(curve.keys[0].value, 2.0);
    // Sampling a curve with coincident keys stays finite.
    assert!(curve.value_at(0.0).is_finite());
}

#[test]
fn cubic_into_a_linear_key_uses_a_zero_arriving_slope() {
    let curve = Curve::from(vec![
        CurveKey::new(
            0.0,
            0.0,
            CurveKeyKind::Cubic {
                left_tangent: 0.0,
                right_tangent: 0.0,
            },
        ),
        CurveKey::new(1.0, 1.0, CurveKeyKind::Linear),
    ]);
    assert!(
        (curve.value_at(0.5) - 0.5).abs() < 1e-5,
        "flat ends: midpoint is the average"
    );
    assert!(curve.value_at(0.25) < 0.25, "eases in");
    assert!(curve.value_at(0.75) > 0.75, "eases out");
}

#[test]
fn linear_key_ignores_the_next_keys_cubic_tangents() {
    let curve = Curve::from(vec![
        CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
        CurveKey::new(
            1.0,
            1.0,
            CurveKeyKind::Cubic {
                left_tangent: 50.0,
                right_tangent: 50.0,
            },
        ),
    ]);
    assert!((curve.value_at(0.25) - 0.25).abs() < 1e-6);
}

#[test]
fn bake_falls_back_to_thirty_hz_for_a_non_positive_rate() {
    let curve = Curve::from(vec![
        CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
        CurveKey::new(1.0, 1.0, CurveKeyKind::Linear),
    ]);
    let zero = curve.bake(0.0, 1.0);
    let thirty = curve.bake(30.0, 1.0);
    assert_eq!(zero, thirty);
    assert_eq!(curve.bake(-5.0, 1.0), thirty);
}

#[test]
fn bake_extends_to_the_last_key_when_the_duration_is_shorter() {
    let curve = Curve::from(vec![
        CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
        CurveKey::new(2.0, 4.0, CurveKeyKind::Linear),
    ]);
    match curve.bake(10.0, 1.0) {
        TrackCurve::Animated { times, values, .. } => {
            assert_eq!(times.len(), values.len());
            assert_eq!(times.first().copied(), Some(0.0));
            let last = times.last().copied().unwrap_or(0.0);
            assert!((last - 2.0).abs() < 1e-5, "ends at the last key: {last}");
            assert!((values.last().copied().unwrap_or(0.0) - 4.0).abs() < 1e-5);
        }
        TrackCurve::Static(_) => panic!("two keys bake to an animated track"),
    }
}

#[test]
fn from_track_of_a_static_track_is_one_linear_key_at_zero() {
    let curve = Curve::from_track(&TrackCurve::Static(1.25));
    assert_eq!(curve.keys, vec![CurveKey::new(0.0, 1.25, CurveKeyKind::Linear)]);
}

#[test]
fn curve_survives_a_ron_round_trip() {
    let curve = Curve::from(vec![
        CurveKey::new(0.0, 0.0, CurveKeyKind::Constant),
        CurveKey::new(0.5, 1.0, CurveKeyKind::cubic_from_angles(0.1, 0.2)),
        CurveKey::new(1.0, 0.0, CurveKeyKind::Linear),
    ]);
    let text = ron::to_string(&curve).expect("serialize");
    let back: Curve = ron::from_str(&text).expect("deserialize");
    assert_eq!(back, curve);
}

/// Audit perf G finding 2: rewriting a parameter that is already stored does not allocate its name.
#[test]
fn perf_g_rewriting_a_parameter_does_not_allocate() {
    let mut p = ParameterSet::new();
    p.set_float("Speed", 0.0);
    p.set_int("Combo", 0);
    p.set_bool("Crouch", false);
    p.set_vec2("Move", Vec2::ZERO);
    p.set_vec3("Aim", Vec3::ZERO);
    p.set_trigger("Jump");
    let before = allocations();
    for i in 1..=1000 {
        p.set_float("Speed", i as f32);
    }
    p.set_int("Combo", 2);
    p.set_bool("Crouch", true);
    p.set_vec2("Move", Vec2::new(1.0, -1.0));
    p.set_vec3("Aim", Vec3::new(0.0, 1.5, -3.0));
    p.set_trigger("Jump");
    let grown = allocations() - before;
    assert_eq!(grown, 0, "rewriting stored parameters allocated {grown} times");
    assert_eq!(p.float("Speed"), 1000.0);
    assert_eq!(p.int("Combo"), 2);
    assert!(p.bool("Crouch"));
    assert_eq!(p.vec2("Move"), Vec2::new(1.0, -1.0));
    assert_eq!(p.vec3("Aim"), Vec3::new(0.0, 1.5, -3.0));
    assert!(p.trigger("Jump"));
}

/// `apply_blending` writes floats and vectors that are already in the set, so it allocates no name.
#[test]
fn perf_g_apply_blending_does_not_allocate() {
    let mut set = ParameterSet::new();
    set.set_float("Speed", 1.0);
    set.set_vec2("Move", Vec2::new(0.25, -0.5));
    set.set_bool("Crouch", true);
    let layout = ParamLayout::of_set(&set);
    let mut lanes = layout.encode(&set);
    let speed_lane = layout.lanes() - 1;
    lanes[speed_lane] = 4.0_f32.to_bits();
    let before = allocations();
    layout.apply_blending(&lanes, &mut set);
    let grown = allocations() - before;
    assert_eq!(grown, 0, "apply_blending allocated {grown} times");
    assert_eq!(set.float("Speed"), 4.0);
    assert_eq!(set.vec2("Move"), Vec2::new(0.25, -0.5));
    assert!(set.bool("Crouch"), "a bool lane is not rewritten by blending");
}

/// The same declarations share one layout, and the second lookup allocates nothing.
#[test]
fn perf_g_declared_layout_is_reused() {
    let graph = speed_graph();
    let first = ParamLayout::declared_shared(&graph);
    let before = allocations();
    let second = ParamLayout::declared_shared(&graph);
    let grown = allocations() - before;
    let same = Arc::ptr_eq(&first, &second);
    assert!(
        same && grown == 0,
        "declared_shared hit: ptr_eq={same} allocations={grown}"
    );
    assert_eq!(first.len(), 3);
}
