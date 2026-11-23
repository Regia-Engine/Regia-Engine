// SPDX-License-Identifier: MPL-2.0
//! Spec 62 § 10 tests 4-9: determinism, detmath (unit tests in the crate), bypass, cancellation,
//! limits and seeds.

mod common;

use std::sync::Arc;

use common::{bowl_heights, bowl_map, box_shape, compile, graph, hex, node, run_with, snapshot};
use parking_lot::Mutex;
use regia_procgen::data::GroundData;
use regia_procgen::exec::{NodeCtx, ProcCache, RunLimitsOverride};
use regia_procgen::nodes::{NodeError, PinSpec, SettingDesc, Settings};
use regia_procgen::{
    Bundle, CancelToken, Evaluator, NodeKind, ProcGraph, ProcNodeRegistry, RunError, RunInputs, RunLimits,
};

/// Run bounds of the acceptance graph: a 200 m × 200 m box at the origin.
const HALF: [f32; 3] = [100.0, 64.0, 100.0];

/// The "rocks on slopes" acceptance graph: ground → slope and altitude filters → jitter → spawn.
pub fn rocks_graph() -> ProcGraph {
    graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node(
                    "sample.ground",
                    &[("spacing_m", "4.0"), ("jitter", "0.8"), ("align", "normal")],
                ),
            ),
            (3, node("density.facing", &[("lo_deg", "1.5"), ("hi_deg", "3.0")])),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"density\""), ("min", "0.5"), ("max", "1.0")],
                ),
            ),
            (
                5,
                node(
                    "filter.range",
                    &[("attr", "\"position:y\""), ("min", "0.5"), ("max", "30.0")],
                ),
            ),
            (
                6,
                node(
                    "points.jitter",
                    &[
                        ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
                        ("scale", "[(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)]"),
                    ],
                ),
            ),
            (
                7,
                node(
                    "spawn.mesh",
                    &[(
                        "entries",
                        "[(mesh: \"meshes/rock_a.glb\", weight: 3.0), (mesh: \"meshes/rock_b.glb\", weight: 1.0)]",
                    )],
                ),
            ),
            (8, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (1, "out", 2, "region"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
            (5, "inside", 6, "in"),
            (6, "out", 7, "in"),
            (7, "out", 8, "in"),
        ],
    )
}

fn bowl_inputs(heights: Vec<f32>, seed: u32) -> (RunInputs, regia_terrain::TerrainMap) {
    let map = bowl_map(heights);
    let bounds = box_shape([0.0; 3], HALF, 0.0);
    let ground = GroundData::capture(&map, [-HALF[0], -HALF[2], HALF[0], HALF[2]], 2);
    (RunInputs::new(snapshot(bounds, Some(ground)), seed), map)
}

/// Recorded output hash of the acceptance graph over the bowl with seed 7 (spec 62 § 6.3). The same
/// value is expected on Linux, Windows, Android aarch64 and macOS.
const ROCKS_HASH: &str = "73b3e8cdd925a98b0af9a35d172514c81dc50fd597f1b8797009fcd4cf8437d8";

#[test]
fn acceptance_graph_is_deterministic_and_places_rocks_on_the_ground() {
    let c = compile(&rocks_graph());
    let (inputs, map) = bowl_inputs(bowl_heights(), 7);
    let a = run_with(&c, &inputs).expect("run");
    let n = a.output.instance_count();
    let counts: Vec<(u32, usize)> = a.report.nodes.iter().map(|l| (l.id, l.elements)).collect();
    assert!(
        n > 200,
        "the filters leave a meaningful band of rocks: {n}; per node {counts:?}"
    );
    // Every instance sits on the terrain (within 1 cm) and lies where the filters allow.
    for set in &a.output.instance_sets {
        for i in &set.instances {
            let [x, y, z] = i.translation;
            assert!(
                (y - map.sample_bilinear(x, z)).abs() <= 0.01,
                "rock at {x},{z} floats: {y}"
            );
            assert!((0.5..=30.0).contains(&y), "altitude filter: {y}");
            assert!(x.abs() <= HALF[0] && z.abs() <= HALF[2], "inside the bounds");
        }
    }
    // Repeat runs, with and without a cache, give the same bytes.
    let b = run_with(&c, &inputs).expect("rerun");
    assert_eq!(a.output_hash, b.output_hash);
    let cache = Arc::new(Mutex::new(ProcCache::new(64 << 20)));
    let cached = Evaluator::with_cache(cache.clone());
    let c1 = cached
        .run(&c, &inputs, &CancelToken::new())
        .expect("cache miss run");
    let c2 = cached
        .run(&c, &inputs, &CancelToken::new())
        .expect("cache hit run");
    assert_eq!(c1.output_hash, a.output_hash);
    assert_eq!(c2.output_hash, a.output_hash);
    assert!(c2.report.cache_hits > 0, "second run hits the cache");
    assert_eq!(hex(&a.output_hash), ROCKS_HASH, "recorded acceptance hash");
}

#[test]
fn seed_changes_output_and_far_terrain_edits_do_not() {
    let c = compile(&rocks_graph());
    let (inputs, _) = bowl_inputs(bowl_heights(), 7);
    let base = run_with(&c, &inputs).expect("run").output_hash;
    let (other_seed, _) = bowl_inputs(bowl_heights(), 8);
    assert_ne!(
        run_with(&c, &other_seed).expect("run").output_hash,
        base,
        "seed changes output"
    );

    // A sample 2.5 samples (5 m) outside the bounds: outside the bilinear and normal stencils.
    let mut far = bowl_heights();
    let col = ((HALF[0] + 255.0) / 2.0) as usize + 3; // world x = 100 m → column 177.5; +3
    far[128 * 256 + col] += 50.0;
    let (far_inputs, _) = bowl_inputs(far, 7);
    assert_eq!(
        run_with(&c, &far_inputs).expect("run").output_hash,
        base,
        "far edit ignored"
    );

    // An edit well inside does change it.
    let mut near = bowl_heights();
    for r in 120..136 {
        for k in 160..176 {
            near[r * 256 + k] += 5.0;
        }
    }
    let (near_inputs, _) = bowl_inputs(near, 7);
    assert_ne!(
        run_with(&c, &near_inputs).expect("run").output_hash,
        base,
        "near edit changes it"
    );
}

#[test]
fn bypass_passes_input_through_and_enabled_jitter_moves_points() {
    let mk = |enabled: bool| {
        let mut g = graph(
            vec![
                (1, node("gen.grid", &[])),
                (
                    2,
                    node(
                        "points.jitter",
                        &[("offset", "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]")],
                    ),
                ),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        g.nodes.get_mut(&2).expect("node").enabled = enabled;
        g
    };
    let snap = snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None);
    let off = common::run(&mk(false), snap.clone(), 1);
    let on = common::run(&mk(true), snap, 1);
    let grid: Vec<[f32; 3]> = common::out_points(&off, "out").position.into_vec();
    assert_eq!(grid.len(), 16);
    assert!(
        grid.iter().all(|p| p[0].fract() == 0.5 || p[0].fract() == -0.5),
        "grid untouched: {grid:?}"
    );
    assert_ne!(common::out_points(&on, "out").position, grid);
    assert!(off.report.node(2).expect("line").bypassed);
}

/// A test node that trips the run's cancel token (spec 62 § 10 test 7).
struct Trip;

impl NodeKind for Trip {
    fn id(&self) -> &str {
        "test.trip"
    }
    fn category(&self) -> &str {
        "test"
    }
    fn doc(&self) -> &str {
        "Cancels the run"
    }
    fn settings(&self) -> &[SettingDesc] {
        &[]
    }
    fn pins(&self, _: &Settings) -> PinSpec {
        PinSpec::new(
            vec![regia_procgen::nodes::PinDesc {
                name: "in".into(),
                kinds: regia_procgen::KindSet::ANY,
                multi: false,
                required: true,
            }],
            vec![regia_procgen::nodes::PinDesc {
                name: "out".into(),
                kinds: regia_procgen::KindSet::ANY,
                multi: false,
                required: false,
            }],
        )
    }
    fn eval(&self, ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
        ctx.cancel.cancel();
        Ok(vec![inputs.first().cloned().unwrap_or_default()])
    }
}

#[test]
fn a_cancel_after_node_two_of_five_returns_cancelled() {
    let mut reg = ProcNodeRegistry::with_builtins();
    reg.register(Arc::new(Trip));
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("test.trip", &[])),
            (3, node("points.jitter", &[])),
            (4, node("points.jitter", &[])),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let c = g.compile(&reg).expect("compile");
    let snap = snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None);
    let r = Evaluator::new().run(&c, &RunInputs::new(snap, 1), &CancelToken::new());
    assert_eq!(r.err(), Some(RunError::Cancelled));
}

#[test]
fn twice_the_point_cap_fails_with_limit_exceeded_naming_points_per_item() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(50.0, 1.0, 40.0)")])),
            (2, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in")],
    );
    let c = compile(&g);
    let mut inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1);
    inputs.limits = RunLimits::DESKTOP.with_override(&RunLimitsOverride {
        points_per_item: Some(1000),
        ..RunLimitsOverride::default()
    });
    match run_with(&c, &inputs) {
        Err(RunError::Node { code, message, .. }) => {
            assert_eq!(code, "limit_exceeded");
            assert!(message.contains("points_per_item"), "{message}");
        }
        other => panic!("expected limit_exceeded, got {other:?}"),
    }
}

#[test]
fn point_seeds_and_draws_ignore_the_cell_and_follow_the_generator_seed() {
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (2, node("sample.ground", &[("spacing_m", "8.0")])),
            (
                3,
                node(
                    "points.jitter",
                    &[("offset", "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]")],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let c = compile(&g);
    let (mut inputs, _) = bowl_inputs(bowl_heights(), 7);
    inputs.cell = Some((0, 0));
    let a = run_with(&c, &inputs).expect("cell 0,0");
    inputs.cell = Some((3, -4));
    let b = run_with(&c, &inputs).expect("cell 3,-4");
    assert_eq!(
        common::out_points(&a, "out"),
        common::out_points(&b, "out"),
        "cell does not matter"
    );
    let (other, _) = bowl_inputs(bowl_heights(), 9);
    let d = run_with(&c, &other).expect("seed 9");
    let (pa, pd) = (common::out_points(&a, "out"), common::out_points(&d, "out"));
    assert_ne!(pa.seed, pd.seed, "generator seed changes seeds");
    assert_ne!(pa.position, pd.position, "and draws");
}

#[test]
fn reseed_changes_every_seed() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("points.reseed", &[])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let snap = snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None);
    let after = common::out_points(&common::run(&g, snap.clone(), 1), "out");
    let mut plain = g.clone();
    plain.nodes.get_mut(&2).expect("node").enabled = false;
    let before = common::out_points(&common::run(&plain, snap, 1), "out");
    assert_eq!(before.len(), after.len());
    assert!(before.seed.iter().zip(&after.seed).all(|(a, b)| a != b));
}

#[test]
fn core_hash_equals_the_grass_hash_for_ten_thousand_inputs() {
    for i in 0..10_000u32 {
        let v = i.wrapping_mul(2_654_435_761) ^ (i << 7);
        assert_eq!(
            regia_core::math::hash::pcg_hash(v),
            regia_terrain::grass::select::pcg_hash(v),
            "input {v}"
        );
    }
}

#[test]
fn a_nan_written_by_a_node_fails_the_run_with_attr_nan() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.set", &[("target", "\"w\""), ("value", "-1.0")])),
            (3, node("attr.math", &[("op", "sqrt"), ("a", "\"w\"")])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let c = compile(&g);
    let r = run_with(
        &c,
        &RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1),
    );
    assert!(
        matches!(r, Err(RunError::Node { ref code, node: 3, .. }) if code == "attr_nan"),
        "{r:?}"
    );
}

#[test]
fn facing_swapped_window_thins_everything() {
    use regia_procgen::{DataItem, Point, PointSet};
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node("density.facing", &[("lo_deg", "40.0"), ("hi_deg", "10.0")]),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let mut pts = PointSet::new();
    pts.push(Point::default());
    let frame = regia_procgen::exec::Frame {
        graph_inputs: [(
            "in".to_string(),
            regia_procgen::Bundle::one(DataItem::points(pts)),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let ri = RunInputs::new(snapshot(box_shape([0.0; 3], [200.0; 3], 0.0), None), 7);
    let r = Evaluator::new()
        .run_frame(&compile(&g), &ri, &CancelToken::new(), &frame)
        .expect("run");
    let b = r.output.graph_outputs.get("out").expect("out");
    assert_eq!(b.0[0].as_points().expect("points").density[0], 0.0);
}
