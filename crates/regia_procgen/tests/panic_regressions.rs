// SPDX-License-Identifier: MPL-2.0
//! Regressions for panics found by the procgen panic review (shipping builds abort on a panic):
//! an inverted `sample.poisson` rectangle, a wide attribute written to an empty set, a NaN spline
//! length, and astronomical spline lengths. No GPU, no goldens.

mod common;

use std::sync::Arc;

use common::{box_shape, compile, graph, node, snapshot};
use regia_core::math::Vec3;
use regia_procgen::data::GroundData;
use regia_procgen::exec::{Frame, StaticSnapshot};
use regia_procgen::nodes::spline_data_item;
use regia_procgen::road::RoadProfile;
use regia_procgen::{Bundle, CancelToken, Evaluator, ProcGraph, RunError, RunInputs, RunResult};
use regia_scene::spline::Spline;

fn run_with_input(g: &ProcGraph, snap: Arc<StaticSnapshot>, input: Bundle) -> Result<RunResult, RunError> {
    let frame = Frame {
        graph_inputs: [("in".to_string(), input)].into_iter().collect(),
        ..Frame::default()
    };
    Evaluator::new().run_frame(&compile(g), &RunInputs::new(snap, 5), &CancelToken::new(), &frame)
}

fn ground_snap() -> Arc<StaticSnapshot> {
    let map = common::flat_map(65, 64.0, 1.0);
    let g = GroundData::capture(&map, [-32.0, -32.0, 32.0, 32.0], 2);
    snapshot(box_shape([0.0; 3], [32.0, 50.0, 32.0], 0.0), Some(g))
}

/// An `in.ground` rect with `min_x > max_x` gives `sample.poisson` a negative grid width: it must
/// return an empty set, and writing `position` of that empty set to item scope must not index.
#[test]
fn poisson_over_an_inverted_rect_is_empty_and_wide_item_attrs_survive_it() {
    let g = graph(
        vec![
            (1, node("in.ground", &[("rect", "[1000.0, 0.0, 32.0, 10.0]")])),
            (2, node("sample.poisson", &[])),
            (
                3,
                node(
                    "attr.set",
                    &[("target", "\"item.saved\""), ("from", "\"position\"")],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let r = run_with_input(&g, ground_snap(), Bundle::empty());
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    };
    let out = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
    for item in &out.0 {
        assert_eq!(item.as_points().map_or(0, regia_procgen::PointSet::len), 0);
    }
}

fn spline_graph(kind: &str, settings: &[(&str, &str)]) -> ProcGraph {
    graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node(kind, settings)),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

fn span(x: f32) -> Bundle {
    Bundle::one(spline_data_item(Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(x, 0.0, 0.0),
    ])))
}

fn nan_span() -> Bundle {
    Bundle::one(spline_data_item(Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(f32::NAN, 0.0, 0.0),
    ])))
}

fn snap() -> Arc<StaticSnapshot> {
    snapshot(box_shape([0.0; 3], [200.0; 3], 0.0), None)
}

#[test]
fn a_nan_spline_is_degenerate_not_a_clamp_abort() {
    let r = run_with_input(&spline_graph("sample.spline", &[]), snap(), nan_span());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn a_nan_road_makes_no_ribbon() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("road.ribbon", &[("profile", "\"roads/p.road.ron\"")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let mut s = StaticSnapshot {
        self_info: snap().self_info.clone(),
        ..StaticSnapshot::default()
    };
    s.road_profiles.insert(
        "roads/p.road.ron".into(),
        RoadProfile::from_ron(
            "RoadProfile(format_version: 1, width_m: 6.0, shoulders_m: (0.5, 0.5), \
             surface_lift_m: 0.05, cross_slope_pct: 2.0, material: \"a.material.ron\", \
             uv_m_per_tile: 4.0, samples_per_m: 1.0)",
        )
        .expect("profile"),
    );
    let r = run_with_input(&g, Arc::new(s), nan_span()).expect("run");
    assert!(r.output.meshes.is_empty());
}

/// A 2^70 m spline made `(0..=n)` and `1..n` collect past `isize::MAX` bytes. They now fail with
/// `limit_exceeded` before allocating.
#[test]
fn an_astronomical_spline_fails_with_a_limit_not_a_capacity_overflow() {
    let huge = 2.0f32.powi(70);
    for g in [
        spline_graph("sample.spline", &[]),
        spline_graph("spline.split", &[("at", "\"every\"")]),
    ] {
        match run_with_input(&g, snap(), span(huge)) {
            Err(RunError::Node { code, .. }) => assert_eq!(code, "limit_exceeded"),
            other => panic!("expected limit_exceeded, got {other:?}"),
        }
    }
}
