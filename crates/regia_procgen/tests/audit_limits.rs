// SPDX-License-Identifier: MPL-2.0
//! Regression tests for the procgen audit (ADR-1816): `points.stamp` checks its output size before
//! allocating (H2), `points.nearest` is bounded and grid-accelerated without changing results (H3),
//! and world-reading nodes never answer from the cache after a terrain edit (H4).

mod common;

use std::sync::Arc;

use common::{bowl_heights, bowl_map, box_shape, compile, graph, node, run_with, snapshot};
use parking_lot::Mutex;
use regia_procgen::data::GroundData;
use regia_procgen::exec::{ProcCache, RunLimitsOverride};
use regia_procgen::{AttrColumn, CancelToken, Evaluator, RunError, RunInputs, RunLimits};

fn inputs_with(points_per_item: usize, total_elements: usize) -> RunInputs {
    let mut inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1);
    inputs.limits = RunLimits::DESKTOP.with_override(&RunLimitsOverride {
        points_per_item: Some(points_per_item),
        total_elements: Some(total_elements),
        ..RunLimitsOverride::default()
    });
    inputs
}

fn limit_message(r: Result<regia_procgen::RunResult, RunError>) -> String {
    match r {
        Err(RunError::Node { code, message, .. }) => {
            assert_eq!(code, "limit_exceeded", "{message}");
            message
        }
        other => panic!("expected limit_exceeded, got {other:?}"),
    }
}

fn stamp_graph(pairing: &str, a: &str, b: &str) -> regia_procgen::ProcGraph {
    graph(
        vec![
            (1, node("gen.grid", &[("count", a)])),
            (2, node("gen.grid", &[("count", b)])),
            (3, node("points.stamp", &[("pairing", pairing)])),
            (4, node("graph.output", &[])),
        ],
        &[
            (1, "out", 3, "source"),
            (2, "out", 3, "targets"),
            (3, "out", 4, "in"),
        ],
    )
}

/// H2: each input is under the per-item cap, the product is not; the node fails before stamping.
#[test]
fn stamp_product_over_the_item_cap_fails_with_limit_exceeded() {
    for pairing in ["by_index", "all_pairs"] {
        let c = compile(&stamp_graph(pairing, "(30.0, 1.0, 30.0)", "(30.0, 1.0, 30.0)"));
        let message = limit_message(run_with(&c, &inputs_with(1000, 1 << 20)));
        assert!(message.contains("points_per_item"), "{message}");
        assert!(message.contains("810000"), "names the stamped size: {message}");
    }
    // Under the cap the stamp runs and emits |source| x |targets| points.
    let c = compile(&stamp_graph("by_index", "(3.0, 1.0, 2.0)", "(5.0, 1.0, 1.0)"));
    let r = run_with(&c, &inputs_with(1000, 1 << 20)).expect("small stamp");
    assert_eq!(common::out_points(&r, "out").len(), 30);
}

/// `points.stamp` writes into the output set. It does not clone the source once per target.
#[test]
fn stamp_does_not_clone_the_source_per_target() {
    let c = compile(&stamp_graph("all_pairs", "(2.0, 1.0, 2.0)", "(6.0, 1.0, 1.0)"));
    regia_procgen::probe::arm();
    let r = run_with(&c, &inputs_with(10_000, 1 << 20)).expect("stamp");
    assert_eq!(common::out_points(&r, "out").len(), 24);
    assert_eq!(
        regia_procgen::probe::stamp_clones(),
        0,
        "the source PointSet was cloned once per target"
    );
}

/// H2: the stamped total is checked against the run's element budget too.
#[test]
fn stamp_total_over_the_element_budget_fails_with_limit_exceeded() {
    let c = compile(&stamp_graph(
        "all_pairs",
        "(20.0, 1.0, 20.0)",
        "(20.0, 1.0, 20.0)",
    ));
    // 400 x 400 = 160 000 points: under the 1 Mi item cap, over a 100 000 element budget.
    let message = limit_message(run_with(&c, &inputs_with(1 << 20, 100_000)));
    assert!(message.contains("total_elements"), "{message}");
}

fn nearest_graph(max_m: &str, a: &str, b: &str) -> regia_procgen::ProcGraph {
    let jitter = || {
        node(
            "points.jitter",
            &[("offset", "[(-0.45, -0.2, -0.45), (0.45, 0.2, 0.45)]")],
        )
    };
    let mut g = graph(
        vec![
            (1, node("gen.grid", &[("count", a)])),
            (
                2,
                node("gen.grid", &[("count", b), ("step_m", "(0.7, 1.0, 0.7)")]),
            ),
            (3, jitter()),
            (4, jitter()),
            (
                5,
                node("points.nearest", &[("max_m", max_m), ("write_vec", "\"nv\"")]),
            ),
            (6, node("graph.output", &[])),
            (7, node("graph.output", &[("pin", "\"to\"")])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 4, "in"),
            (3, "out", 5, "in"),
            (4, "out", 5, "to"),
            (5, "out", 6, "in"),
            (4, "out", 7, "in"),
        ],
    );
    // Keep the two jitters' draws apart.
    for (id, salt) in [(3u32, 11u32), (4, 12)] {
        if let Some(n) = g.nodes.get_mut(&id) {
            n.seed_salt = Some(salt);
        }
    }
    g
}

/// H3: the grid search gives exactly the full scan's answer (first strictly nearest index wins).
#[test]
fn nearest_with_the_grid_matches_a_full_scan() {
    for max_m in ["0.6", "1.5", "1000.0"] {
        let c = compile(&nearest_graph(max_m, "(12.0, 1.0, 9.0)", "(14.0, 1.0, 11.0)"));
        let r = run_with(&c, &inputs_with(1 << 20, 1 << 22)).expect("nearest");
        let p = common::out_points(&r, "out");
        let to = common::out_points(&r, "to");
        let Some(AttrColumn::Float(dist)) = p.attrs.get("nearest_dist") else {
            panic!("nearest_dist written");
        };
        let Some(AttrColumn::Vec3(vecs)) = p.attrs.get("nv") else {
            panic!("nv written");
        };
        let m: f32 = max_m.parse().expect("max_m");
        let mut within = 0;
        for (i, a) in p.position.iter().enumerate() {
            let mut best: Option<(f32, usize)> = None;
            for (j, b) in to.position.iter().enumerate() {
                let d: f32 = (0..3).map(|k| (a[k] - b[k]) * (a[k] - b[k])).sum();
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, j));
                }
            }
            match best.filter(|(d, _)| d.sqrt() <= m) {
                Some((d, j)) => {
                    within += 1;
                    assert_eq!(dist[i], f64::from(d.sqrt()), "point {i}, max_m {max_m}");
                    let want: [f64; 3] = std::array::from_fn(|k| f64::from(to.position[j][k] - a[k]));
                    assert_eq!(vecs[i], want, "point {i}, max_m {max_m}");
                }
                None => assert_eq!(dist[i], f64::from(m), "point {i}, max_m {max_m}"),
            }
        }
        assert!(within > 0, "some points find a neighbour at max_m {max_m}");
    }
}

/// H3: a search that would run more distance tests than the cap fails instead of running for hours.
#[test]
fn nearest_over_the_test_cap_fails_with_limit_exceeded() {
    // 2048 x 2048 tests with every target in reach > 2048 x 1024 allowed.
    let c = compile(&nearest_graph(
        "1000000000.0",
        "(64.0, 1.0, 32.0)",
        "(64.0, 1.0, 32.0)",
    ));
    let message = limit_message(run_with(&c, &inputs_with(2048, 1 << 22)));
    assert!(message.contains("points.nearest"), "{message}");
    // With a reach of a few cells the same sets stay well under the cap.
    let c = compile(&nearest_graph("1.0", "(64.0, 1.0, 32.0)", "(64.0, 1.0, 32.0)"));
    assert!(run_with(&c, &inputs_with(2048, 1 << 22)).is_ok());
}

/// H4: one shared cache, the same graph and seed, an edited terrain: the second run must match an
/// uncached run over the edited terrain, not the cached pre-edit output.
#[test]
fn a_terrain_edit_never_answers_from_the_cache() {
    let half = [100.0f32, 64.0, 100.0];
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node("sample.ground", &[("spacing_m", "4.0"), ("align", "normal")]),
            ),
            (3, node("points.align", &[("to", "ground_normal")])),
            (4, node("spawn.mesh", &[("entries", "[(mesh: \"m.glb\")]")])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let c = compile(&g);
    let inputs = |heights: Vec<f32>| {
        let map = bowl_map(heights);
        let ground = GroundData::capture(&map, [-half[0], -half[2], half[0], half[2]], 2);
        RunInputs::new(snapshot(box_shape([0.0; 3], half, 0.0), Some(ground)), 7)
    };
    let mut edited = bowl_heights();
    for r in 120..136 {
        for k in 120..136 {
            edited[r * 256 + k] += 5.0;
        }
    }
    let (before, after) = (inputs(bowl_heights()), inputs(edited));
    let fresh_after = Evaluator::new()
        .run(&c, &after, &CancelToken::new())
        .expect("uncached");
    let cache = Arc::new(Mutex::new(ProcCache::new(64 << 20)));
    let first = Evaluator::with_cache(cache.clone())
        .run(&c, &before, &CancelToken::new())
        .expect("first");
    assert_ne!(
        first.output_hash, fresh_after.output_hash,
        "the edit moves points"
    );
    let second = Evaluator::with_cache(cache.clone())
        .run(&c, &after, &CancelToken::new())
        .expect("second");
    assert_eq!(second.output_hash, fresh_after.output_hash, "no stale cache hit");
    // An identical snapshot (a new capture of the same terrain) still hits the cache.
    let again = Evaluator::with_cache(cache)
        .run(&c, &inputs(bowl_heights()), &CancelToken::new())
        .expect("again");
    assert_eq!(again.output_hash, first.output_hash);
    assert!(
        again.report.cache_hits >= 2,
        "ground and align hit: {}",
        again.report.cache_hits
    );
}

/// `points.repeat` `chain: false` applies the step k times in the original frame (spec 63): with a
/// yaw step and a +x offset the copies stay on a straight line; `chain: true` curves away from it.
#[test]
fn repeat_unchained_steps_in_the_original_frame() {
    let run_mode = |chain: &str| {
        let g = graph(
            vec![
                (1, node("gen.grid", &[("count", "(1.0, 1.0, 1.0)")])),
                (
                    2,
                    node(
                        "points.repeat",
                        &[
                            ("count", "2"),
                            ("offset", "(1.0, 0.0, 0.0)"),
                            ("rot_deg", "(0.0, 90.0, 0.0)"),
                            ("chain", chain),
                        ],
                    ),
                ),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        run_with(&compile(&g), &inputs_with(1 << 20, 1 << 22)).expect("repeat")
    };
    let (a, b) = (run_mode("true"), run_mode("false"));
    let (pa, pb) = (common::out_points(&a, "out"), common::out_points(&b, "out"));
    assert_eq!((pa.len(), pb.len()), (3, 3));
    let (base, c2, u2) = (pb.point(0).position, pa.point(2).position, pb.point(2).position);
    // Unchained: copy 2 is two offsets along the original x axis.
    assert!(
        (u2[0] - base[0] - 2.0).abs() < 1e-4 && (u2[2] - base[2]).abs() < 1e-4,
        "{u2:?}"
    );
    // Chained: the rotated frame turns the second step off the x axis.
    assert!((c2[2] - base[2]).abs() > 0.5, "{c2:?}");
}

/// M7: non-finite `spawn.mesh` entry numbers fail compile with `setting_type` (they used to bake
/// inf translations and draw distances).
#[test]
fn spawn_entries_with_non_finite_numbers_do_not_compile() {
    let compile_entries = |entries: &str| {
        let g = graph(
            vec![
                (1, node("gen.one", &[])),
                (2, node("spawn.mesh", &[("entries", entries)])),
            ],
            &[(1, "out", 2, "in")],
        );
        g.compile(&regia_procgen::ProcNodeRegistry::with_builtins())
    };
    for bad in [
        "[(mesh: \"m.glb\", offset: (t: (inf, 0.0, 0.0)))]",
        "[(mesh: \"m.glb\", offset: (s: (1.0, NaN, 1.0)))]",
        "[(mesh: \"m.glb\", draw_distance_m: inf)]",
        "[(mesh: \"m.glb\", fade_m: 1e300)]",
    ] {
        let errors = compile_entries(bad).expect_err(bad);
        assert!(errors.has("setting_type"), "{bad}: {:?}", errors.0);
    }
    assert!(
        compile_entries("[(mesh: \"m.glb\", offset: (t: (1.0, 0.0, 0.0)), draw_distance_m: 50.0)]").is_ok()
    );
}

/// M4: an `in.rays` grid over the cap (the capture casts none) fails with `limit_exceeded` naming
/// the count, instead of an empty run that reports success.
#[test]
fn an_over_cap_ray_grid_fails_instead_of_running_empty() {
    let g = graph(
        vec![
            (1, node("in.rays", &[("spacing_m", "2.0")])),
            (2, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in")],
    );
    let c = compile(&g);
    // 4200 m square at 2 m: 2100 x 2100 = 4 410 000 rays > 4 Mi.
    let big = RunInputs::new(
        snapshot(box_shape([0.0; 3], [2100.0, 10.0, 2100.0], 0.0), None),
        1,
    );
    let message = limit_message(run_with(&c, &big));
    assert!(message.contains("4410000"), "{message}");
    // A grid under the cap runs (no physics hits in the snapshot: empty output).
    let small = RunInputs::new(snapshot(box_shape([0.0; 3], [100.0, 10.0, 100.0], 0.0), None), 1);
    assert!(run_with(&c, &small).is_ok());
}

/// M8: a float key matches the same text in `spawn.*` as `items.partition` and `attr.lookup` name
/// its group (`[1.0]`); spawn used to see `""` and match nothing.
#[test]
fn float_keys_match_in_spawn_like_in_partition() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
            (2, node("attr.set", &[("target", "\"biome\""), ("value", "1.0")])),
            (
                3,
                node(
                    "spawn.mesh",
                    &[
                        ("pick", "ByAttr(attr: \"biome\")"),
                        ("entries", "[(mesh: \"m\", key: \"[1.0]\")]"),
                    ],
                ),
            ),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = run_with(&compile(&g), &inputs_with(1 << 20, 1 << 22)).expect("spawn");
    assert_eq!(r.output.instance_count(), 3);
    assert_eq!(regia_procgen::AttrValue::Float(1.0).key_text(), "[1.0]");
    assert_eq!(regia_procgen::AttrValue::Int(2).key_text(), "2");
}

/// `flow.quality` whose `high` pin hangs on an inactive `flow.if` branch or is not wired, with a
/// small grid on `low`.
fn quality_graph(wire_high: bool) -> regia_procgen::ProcGraph {
    let mut edges = vec![(1, "out", 2, "in"), (1, "out", 4, "low"), (4, "out", 5, "in")];
    if wire_high {
        edges.push((2, "else", 4, "high"));
    }
    graph(
        vec![
            (1, node("gen.grid", &[("count", "(2.0, 1.0, 2.0)")])),
            (2, node("flow.if", &[("value", "true")])),
            (4, node("flow.quality", &[])),
            (5, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &edges,
    )
}

/// g1 H1: an unwired pin and a wired empty pin give the same bundles, and `flow.quality` picks by
/// the wires; through one cache the second run must not reuse the first choice.
#[test]
fn flow_quality_never_reuses_a_choice_made_with_other_wires() {
    let mut inputs = inputs_with(1 << 20, 1 << 22);
    inputs.quality = regia_render::quality::QualityTier::High;
    let run = |wire_high: bool, cache: Option<&Arc<Mutex<ProcCache>>>| {
        let c = compile(&quality_graph(wire_high));
        let e = cache.map_or_else(Evaluator::new, |x| Evaluator::with_cache(x.clone()));
        e.run(&c, &inputs, &CancelToken::new()).expect("run")
    };
    assert_eq!(
        common::out_points(&run(true, None), "out").len(),
        0,
        "the wired high pin wins"
    );
    assert_eq!(
        common::out_points(&run(false, None), "out").len(),
        4,
        "low when high is unwired"
    );
    let cache = Arc::new(Mutex::new(ProcCache::new(64 << 20)));
    assert_eq!(common::out_points(&run(true, Some(&cache)), "out").len(), 0);
    assert_eq!(
        common::out_points(&run(false, Some(&cache)), "out").len(),
        4,
        "the cached choice was made with other wires"
    );
    assert_eq!(common::out_points(&run(true, Some(&cache)), "out").len(), 0);
}

/// g1 H3: both axes saturate `i64`, so the old span wrapped to zero, passed the check and walked
/// every `i64`. Each sampler must refuse the count before its first cell.
#[test]
fn a_sampler_over_a_huge_box_fails_before_its_first_cell() {
    for (kind, settings) in [
        ("sample.region", vec![("spacing_m", "0.01")]),
        ("sample.poisson", vec![("min_dist_m", "0.05")]),
        ("sample.volume", vec![("cell_m", "(0.01, 0.01, 0.01)")]),
    ] {
        let g = graph(
            vec![
                (1, node("in.self", &[])),
                (2, node(kind, &settings)),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        );
        let c = compile(&g);
        let huge = snapshot(box_shape([0.0; 3], [1.0e20; 3], 0.0), None);
        let message = limit_message(run_with(&c, &RunInputs::new(huge, 1)));
        assert!(message.contains("points_per_item"), "{kind}: {message}");
    }
}

/// g1 L8: the cell range is half-open, so the keep test is too: a center exactly on the max edge
/// belongs to the next rectangle, which is what tile ownership (`in_half_open`) says.
#[test]
fn a_grid_center_on_the_max_edge_is_left_to_the_next_rectangle() {
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node("sample.region", &[("spacing_m", "8.0"), ("jitter", "0.0")]),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in")],
    );
    let inputs = RunInputs::new(
        snapshot(box_shape([30.0, 0.0, 30.0], [30.0, 5.0, 30.0], 0.0), None),
        1,
    );
    let r = run_with(&compile(&g), &inputs).expect("region");
    let p = common::out_points(&r, "out");
    // Cell centers 4, 12, ..., 52 are inside [0, 60); 60 is on the edge.
    assert_eq!(p.len(), 7 * 7);
    assert!(p.position.iter().all(|q| q[0] < 60.0 && q[2] < 60.0));
}

/// g1 M6: spec 62 § 6.2, a created point's seed follows the generator seed.
#[test]
fn an_enclosing_point_is_seeded_by_the_generator() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(3.0, 1.0, 3.0)")])),
            (2, node("points.enclose", &[])),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let seed_of = |gen_seed: u32| {
        let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), gen_seed);
        common::out_points(&run_with(&compile(&g), &inputs).expect("enclose"), "out").seed[0]
    };
    assert_eq!(seed_of(1), seed_of(1));
    assert_ne!(seed_of(1), seed_of(2));
}

/// g1 M5: `in.table` is where a file's numbers enter the run; every comparison with NaN is false, so
/// a NaN cell used to steer `flow.if` and pass `debug.check`.
#[test]
fn a_non_finite_table_cell_fails_the_run_with_input_nan() {
    use regia_procgen::data::{AttrColumn, Table};
    let g = graph(
        vec![
            (1, node("in.table", &[("asset", "\"t.table.ron\"")])),
            (2, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in")],
    );
    let c = compile(&g);
    let run = |column: AttrColumn| {
        let mut snap = (*snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None)).clone();
        let table = Table {
            rows: 2,
            columns: vec![("w".to_string(), column)],
        };
        snap.tables.insert("t.table.ron".to_string(), table);
        run_with(&c, &RunInputs::new(Arc::new(snap), 1))
    };
    assert!(run(AttrColumn::Float(vec![1.0, 2.0].into())).is_ok());
    for (column, row) in [
        (AttrColumn::Float(vec![1.0, f64::NAN].into()), 1),
        (
            AttrColumn::Vec3(vec![[f64::INFINITY, 0.0, 0.0], [0.0; 3]].into()),
            0,
        ),
    ] {
        match run(column) {
            Err(RunError::Node { code, message, .. }) => {
                assert_eq!(code, "input_nan", "{message}");
                assert!(message.contains(&format!("row {row}")), "{message}");
            }
            other => panic!("expected input_nan, got {other:?}"),
        }
    }
}
