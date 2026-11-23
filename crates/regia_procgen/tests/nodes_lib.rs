// SPDX-License-Identifier: MPL-2.0
//! Spec 63 § 14 required node cases (no GPU, small hand-built inputs).

mod common;

use std::collections::BTreeSet;

use common::{box_shape, compile, flat_map, graph, node, out_points, run, run_with, snapshot};
use regia_procgen::data::GroundData;
use regia_procgen::{ProcNodeRegistry, RunError, RunInputs};

fn ground_snapshot(
    map: &regia_terrain::TerrainMap,
    half: f32,
) -> std::sync::Arc<regia_procgen::StaticSnapshot> {
    let g = GroundData::capture(map, [-half, -half, half, half], 2);
    snapshot(box_shape([0.0; 3], [half, 50.0, half], 0.0), Some(g))
}

/// `in.ground` with an explicit rect → `sample.ground` → output.
fn ground_graph(rect: [f32; 4], spacing: f32, jitter: f32) -> regia_procgen::ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "in.ground",
                    &[(
                        "rect",
                        &format!("[{:?}, {:?}, {:?}, {:?}]", rect[0], rect[1], rect[2], rect[3]),
                    )],
                ),
            ),
            (
                2,
                node(
                    "sample.ground",
                    &[
                        ("spacing_m", &format!("{spacing:?}")),
                        ("jitter", &format!("{jitter:?}")),
                    ],
                ),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in")],
    )
}

#[test]
fn case_01_sample_ground_grid_jitter_and_union_of_halves() {
    let map = flat_map(65, 64.0, 5.0);
    let snap = ground_snapshot(&map, 32.0);
    let p = out_points(
        &run(
            &ground_graph([-32.0, -32.0, 32.0, 32.0], 4.0, 0.0),
            snap.clone(),
            3,
        ),
        "out",
    );
    assert_eq!(p.len(), 256);
    for q in &p.position {
        assert_eq!(q[1], 5.0);
        assert_eq!((q[0] + 2.0).rem_euclid(4.0), 0.0, "cell center x {q:?}");
        assert_eq!((q[2] + 2.0).rem_euclid(4.0), 0.0, "cell center z {q:?}");
    }
    let j = out_points(
        &run(
            &ground_graph([-32.0, -32.0, 32.0, 32.0], 4.0, 0.8),
            snap.clone(),
            3,
        ),
        "out",
    );
    assert_eq!(j.len(), 256);
    for (a, b) in p.position.iter().zip(&j.position) {
        assert!(
            (a[0] - b[0]).abs() < 2.0 && (a[2] - b[2]).abs() < 2.0,
            "stays in its cell"
        );
    }
    let key = |v: &[[f32; 3]]| {
        v.iter()
            .map(|q| (q[0].to_bits(), q[2].to_bits()))
            .collect::<BTreeSet<_>>()
    };
    let left = out_points(
        &run(
            &ground_graph([-32.0, -32.0, 0.0, 32.0], 4.0, 0.8),
            snap.clone(),
            3,
        ),
        "out",
    );
    let right = out_points(
        &run(&ground_graph([0.0, -32.0, 32.0, 32.0], 4.0, 0.8), snap, 3),
        "out",
    );
    let mut union = key(&left.position);
    union.extend(key(&right.position));
    assert_eq!(left.len() + right.len(), 256, "halves do not overlap");
    assert_eq!(
        union,
        key(&j.position),
        "two halves = one whole (world-aligned grid, position seeds)"
    );
}

#[test]
fn case_02_poisson_respects_min_dist_and_is_repeatable() {
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node("sample.poisson", &[("min_dist_m", "1.0"), ("attempts", "6")]),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in")],
    );
    let snap = snapshot(box_shape([0.0; 3], [40.0, 5.0, 40.0], 0.0), None);
    let a = out_points(&run(&g, snap.clone(), 11), "out");
    assert!(a.len() >= 1500, "dense enough to matter: {}", a.len());
    let pts: Vec<[f32; 3]> = a.position.iter().take(5000).copied().collect();
    for i in 0..pts.len() {
        for j in i + 1..pts.len() {
            let (dx, dz) = (pts[i][0] - pts[j][0], pts[i][2] - pts[j][2]);
            assert!(dx * dx + dz * dz >= 1.0 - 1e-4, "pair {i},{j} too close");
        }
    }
    assert_eq!(out_points(&run(&g, snap, 11), "out"), a, "same seed, same points");
}

fn density_graph(
    density: &str,
    n: [u32; 2],
    node_ty: &str,
    settings: &[(&str, &str)],
) -> regia_procgen::ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", &format!("({}.0, 1.0, {}.0)", n[0], n[1]))],
                ),
            ),
            (
                2,
                node("attr.set", &[("target", "\"density\""), ("value", density)]),
            ),
            (3, node(node_ty, settings)),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    )
}

fn empty_snap() -> std::sync::Arc<regia_procgen::StaticSnapshot> {
    snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None)
}

#[test]
fn case_03_density_remap() {
    let remap = |d: &str, s: &[(&str, &str)]| {
        out_points(
            &run(&density_graph(d, [1, 1], "density.remap", s), empty_snap(), 1),
            "out",
        )
        .density[0]
    };
    let ft = [("from", "[0.2, 0.6]"), ("to", "[0.1, 0.9]")];
    assert!((remap("0.2", &ft) - 0.1).abs() < 1e-6, "low end");
    assert!((remap("0.6", &ft) - 0.9).abs() < 1e-6, "high end");
    assert!(
        (remap("0.4", &[("from", "[0.3, 0.3]"), ("to", "[0.2, 0.8]")]) - 0.5).abs() < 1e-6,
        "collapsed"
    );
    let keep = [("from", "[0.0, 0.5]"), ("to", "[0.0, 0.2]"), ("outside", "keep")];
    assert!((remap("0.9", &keep) - 0.9).abs() < 1e-6);
    let re = [("from", "[0.0, 0.5]"), ("to", "[0.0, 0.2]"), ("outside", "remap")];
    assert!((remap("0.9", &re) - 0.36).abs() < 1e-6);
}

#[test]
fn case_04_realize_random_keeps_about_the_density() {
    let g = density_graph("0.3", [400, 250], "density.realize", &[("mode", "random")]);
    let p = out_points(&run(&g, empty_snap(), 5), "out");
    assert!((29_000..=31_000).contains(&p.len()), "kept {}", p.len());
    assert!(p.density.iter().all(|d| *d == 1.0));
}

#[test]
fn case_05_prune_keeps_no_overlaps_and_the_largest() {
    // Mixed box sizes on a dense grid.
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(30.0, 1.0, 30.0)"), ("step_m", "(0.7, 1.0, 0.7)")],
                ),
            ),
            (
                2,
                node(
                    "points.jitter",
                    &[("scale", "[(0.5, 0.5, 0.5), (2.0, 2.0, 2.0)]")],
                ),
            ),
            (3, node("points.prune", &[])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let r = run(&g, empty_snap(), 2);
    let kept = out_points(&r, "out");
    assert!(kept.len() > 10 && kept.len() < 900);
    for i in 0..kept.len() {
        for j in i + 1..kept.len() {
            let (a, b) = (kept.world_aabb(i), kept.world_aabb(j));
            let overlap = (0..3).all(|k| a.0[k] < b.1[k] && b.0[k] < a.1[k]);
            assert!(!overlap, "kept boxes {i} and {j} overlap");
        }
    }
    let mut no_prune = g.clone();
    no_prune.nodes.get_mut(&3).expect("node").enabled = false;
    let all = out_points(&run(&no_prune, empty_snap(), 2), "out");
    let largest = (0..all.len())
        .max_by(|&a, &b| all.scale[a][0].total_cmp(&all.scale[b][0]))
        .expect("some");
    assert!(kept.position.contains(&all.position[largest]), "largest kept");

    // Equal boxes: `random` with equal_within differs from `as_is`.
    let eq = |order: &str| {
        let g = graph(
            vec![
                (
                    1,
                    node(
                        "gen.grid",
                        &[("count", "(20.0, 1.0, 20.0)"), ("step_m", "(0.6, 1.0, 0.6)")],
                    ),
                ),
                (
                    2,
                    node("points.prune", &[("order", order), ("equal_within", "0.05")]),
                ),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        out_points(&run(&g, empty_snap(), 2), "out").position
    };
    assert_ne!(eq("random"), eq("as_is"));
}

#[test]
fn case_06_stamp_pairing() {
    let g = |src_items: usize| {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for k in 0..src_items as u32 {
            nodes.push((
                20 + k,
                node(
                    "gen.points",
                    &[("points", "[((0.0, 0.0, 0.0)), ((0.0, 0.0, 1.0))]")],
                ),
            ));
            edges.push((20 + k, "out", 1, "in"));
        }
        // Sources merged into a multi-item bundle through items.gather; three target items.
        nodes.push((1, node("items.gather", &[])));
        nodes.push((30, node("gen.points", &[("points", "[((5.0, 0.0, 0.0))]")])));
        nodes.push((31, node("gen.points", &[("points", "[((6.0, 0.0, 0.0))]")])));
        nodes.push((32, node("gen.points", &[("points", "[((7.0, 0.0, 0.0))]")])));
        nodes.push((2, node("items.gather", &[])));
        edges.extend([(30, "out", 2, "in"), (31, "out", 2, "in"), (32, "out", 2, "in")]);
        nodes.push((3, node("points.stamp", &[])));
        nodes.push((4, node("graph.output", &[])));
        edges.extend([
            (1, "out", 3, "source"),
            (2, "out", 3, "targets"),
            (3, "out", 4, "in"),
        ]);
        graph(nodes, &edges)
    };
    let c = compile(&g(2));
    match run_with(&c, &RunInputs::new(empty_snap(), 1)) {
        Err(RunError::Node { code, .. }) => assert_eq!(code, "stamp_count_mismatch"),
        other => panic!("expected stamp_count_mismatch, got {other:?}"),
    }
    let r = run(&g(1), empty_snap(), 1);
    let items = &r.output.graph_outputs["out"].0;
    assert_eq!(items.len(), 3);
    let xs: Vec<f32> = items
        .iter()
        .flat_map(|i| i.as_points().expect("points").position.iter().map(|p| p[0]))
        .collect();
    assert_eq!(xs, vec![5.0, 5.0, 6.0, 6.0, 7.0, 7.0], "target-major order");
}

#[test]
fn case_07_spawn_mesh_weights_and_by_attr() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(200.0, 1.0, 200.0)")])),
            (
                2,
                node(
                    "spawn.mesh",
                    &[(
                        "entries",
                        "[(mesh: \"a\", weight: 1.0), (mesh: \"b\", weight: 1.0), (mesh: \"c\", weight: 1.0), (mesh: \"d\", weight: 1.0), (mesh: \"z\", weight: 0.0)]",
                    )],
                ),
            ),
        ],
        &[(1, "out", 2, "in")],
    );
    let r = run(&g, empty_snap(), 4);
    let sets = &r.output.instance_sets;
    assert_eq!(sets.len(), 4, "weight 0 never spawns");
    for s in sets {
        assert!(
            (9_700..=10_300).contains(&s.instances.len()),
            "{}: {}",
            s.mesh,
            s.instances.len()
        );
    }
    let by = graph(
        vec![
            (1, node("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
            (
                2,
                node("attr.set", &[("target", "\"kind\""), ("value", "\"rock\"")]),
            ),
            (
                3,
                node(
                    "spawn.mesh",
                    &[
                        ("pick", "ByAttr(attr: \"kind\")"),
                        ("entries", "[(mesh: \"m\", key: \"tree\")]"),
                    ],
                ),
            ),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = run(&by, empty_snap(), 4);
    assert_eq!(r.output.instance_count(), 0);
    assert_eq!(
        r.report.node(3).expect("line").counters.get("unmatched"),
        Some(&3)
    );
}

#[test]
fn case_08_flow_if_inactive_branch_and_choose() {
    let g = graph(
        vec![
            (1, node("gen.one", &[])),
            (2, node("flow.if", &[("value", "false")])),
            (3, node("points.jitter", &[])),
            (4, node("graph.output", &[("pin", "\"then\"")])),
            (5, node("graph.output", &[("pin", "\"else\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "then", 3, "in"),
            (3, "out", 4, "in"),
            (2, "else", 5, "in"),
        ],
    );
    let r = run(&g, empty_snap(), 1);
    assert!(
        r.report.node(3).expect("line").inactive,
        "untaken branch is inactive"
    );
    assert!(r.report.node(4).expect("line").inactive);
    assert!(!r.output.graph_outputs.contains_key("then"));
    assert_eq!(out_points(&r, "else").len(), 1);

    let ch = graph(
        vec![
            (1, node("gen.one", &[])),
            (2, node("gen.grid", &[])),
            (3, node("flow.choose", &[("value", "true")])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "a"), (2, "out", 3, "b"), (3, "out", 4, "in")],
    );
    let r = run(&ch, empty_snap(), 1);
    assert_eq!(out_points(&r, "out").len(), 1);
    assert!(!r.report.node(2).expect("line").inactive, "both inputs computed");
    assert_eq!(r.report.node(2).expect("line").elements, 16);
}

#[test]
fn case_11_failing_check_fails_the_run_and_publishes_nothing() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("debug.check", &[("max_count", "10")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let c = compile(&g);
    match run_with(&c, &RunInputs::new(empty_snap(), 1)) {
        Err(RunError::Node { code, node, .. }) => {
            assert_eq!(code, "check_failed");
            assert_eq!(node, 2);
        }
        other => panic!("expected check_failed, got {other:?}"),
    }
}

#[test]
fn case_13_by_index_list() {
    let (sel, odd) = regia_procgen::nodes::parse_index_list("0, 2, 4..6, 7.., -1", 10).expect("parse");
    assert_eq!(sel, vec![0, 2, 4, 5, 7, 8, 9]);
    assert!(odd, "-1 duplicates 9 and is reported");
    assert!(regia_procgen::nodes::parse_index_list("a..", 3).is_err());
}

/// Audit H1: huge ranges are clipped arithmetically (they used to iterate every integer, hanging
/// compile and run), and extreme numbers neither overflow nor panic.
#[test]
fn by_index_huge_ranges_are_bounded_by_the_item_count() {
    use regia_procgen::nodes::parse_index_list;
    let (sel, odd) = parse_index_list("0..99999999999", 4).expect("parse");
    assert_eq!(sel, vec![0, 1, 2, 3]);
    assert!(odd, "the range runs past the items");
    // The compile-time validator parses with n = 0.
    let (sel, odd) = parse_index_list(
        "0..9223372036854775807, -9223372036854775808..=9223372036854775807",
        0,
    )
    .expect("parse");
    assert!(sel.is_empty());
    assert!(odd);
    let (sel, odd) = parse_index_list("-9223372036854775808..=9223372036854775807", 3).expect("parse");
    assert_eq!(sel, vec![0, 1, 2]);
    assert!(odd);
    let (sel, odd) = parse_index_list("5..3, 1..=1", 4).expect("parse");
    assert_eq!(sel, vec![1]);
    assert!(!odd, "an empty range is neither a duplicate nor out of range");
    // Many overlapping entries cost one step each plus the items.
    let many = vec!["0..1000000000000"; 100_000].join(",");
    let (sel, odd) = parse_index_list(&many, 3).expect("parse");
    assert_eq!(sel, vec![0, 1, 2]);
    assert!(odd);
    let (sel, odd) = parse_index_list("1..3, 2", 4).expect("parse");
    assert_eq!(sel, vec![1, 2]);
    assert!(odd, "2 duplicates the range");
}

#[test]
fn case_14_region_subtract_cut_and_scale() {
    let g = |mode: &str| {
        graph(
            vec![
                (
                    1,
                    node(
                        "gen.grid",
                        &[("count", "(9.0, 1.0, 1.0)"), ("step_m", "(1.0, 1.0, 1.0)")],
                    ),
                ),
                (2, node("in.self", &[])),
                (
                    3,
                    node("region.subtract", &[("mode", mode), ("keep_zero", "true")]),
                ),
                (4, node("graph.output", &[])),
            ],
            &[(1, "out", 3, "in"), (2, "out", 3, "minus"), (3, "out", 4, "in")],
        )
    };
    // Grid x = -4..4; box half-extent 1.5 → inside: -1, 0, 1. Edge band 2 m for the scale case.
    let snap = snapshot(box_shape([0.0; 3], [1.5, 1.0, 1.5], 0.0), None);
    let cut = out_points(&run(&g("cut"), snap, 1), "out");
    let xs: Vec<f32> = cut.position.iter().map(|p| p[0]).collect();
    assert_eq!(xs, vec![-4.0, -3.0, -2.0, 2.0, 3.0, 4.0]);
    let snap = snapshot(box_shape([0.0; 3], [1.5, 1.0, 1.5], 2.0), None);
    let scaled = out_points(&run(&g("scale"), snap, 1), "out");
    // x = 2.5 is the midpoint of the band [1.5, 3.5]: region 0.5 → density 0.5. Grid has x = 2 and 3:
    // region 0.75 and 0.25.
    let at = |x: f32| {
        scaled
            .position
            .iter()
            .position(|p| p[0] == x)
            .map(|i| scaled.density[i])
            .expect("point")
    };
    assert!((at(2.0) - 0.25).abs() < 1e-6);
    assert!((at(3.0) - 0.75).abs() < 1e-6);
    assert_eq!(at(0.0), 0.0);
    assert_eq!(at(4.0), 1.0);
}

#[test]
fn case_15_flat_only() {
    let check = |heights: Vec<f32>| {
        let map = regia_terrain::TerrainMap::from_heights(65, 65, heights, 64.0, 1.0, 0.0).expect("map");
        let snap = ground_snapshot(&map, 32.0);
        let g = graph(
            vec![
                (1, node("gen.points", &[("points", "[((-0.5, 0.0, 0.5))]")])),
                (2, node("in.ground", &[])),
                (3, node("points.flat_only", &[])),
                (4, node("graph.output", &[])),
            ],
            &[(1, "out", 3, "in"), (2, "out", 3, "ground"), (3, "out", 4, "in")],
        );
        out_points(&run(&g, snap, 1), "out").len()
    };
    assert_eq!(check(vec![3.0; 65 * 65]), 1, "plane");
    assert_eq!(
        check((0..65 * 65).map(|i| (i % 65) as f32 * 0.5).collect()),
        1,
        "uniform slope"
    );
    assert_eq!(
        check(
            (0..65 * 65)
                .map(|i| if i % 65 > 32 { 2.0 } else { 0.0 })
                .collect()
        ),
        0,
        "step"
    );
}

#[test]
fn every_builtin_node_type_validates_its_defaults_and_round_trips_settings() {
    let reg = ProcNodeRegistry::with_builtins();
    assert!(reg.len() >= 40, "registry has {} types", reg.len());
    for kind in reg.iter() {
        // Defaults parse and validate (a node with no settings compiles alone when it has no required pins).
        for d in kind.settings() {
            let v = regia_procgen::SettingValue::parse(d.default).expect("default parses");
            assert_eq!(
                regia_procgen::SettingValue::parse(&v.write()).expect("reparse"),
                v,
                "{}.{}",
                kind.id(),
                d.name
            );
        }
        // One bad setting → its error code.
        if let Some(d) = kind.settings().iter().find(|d| d.min.is_some()) {
            let g = graph(vec![(1, node(kind.id(), &[(d.name, "1e15")]))], &[]);
            let e = g.compile(&reg).expect_err("out of range");
            assert!(
                e.has("setting_out_of_range"),
                "{}.{}: {:?}",
                kind.id(),
                d.name,
                e.0
            );
        }
    }
}

#[test]
fn case_12_lookup_weighted_multi_match_follows_the_weights() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(100.0, 1.0, 100.0)")])),
            (
                2,
                node("attr.set", &[("target", "\"kind\""), ("value", "\"rock\"")]),
            ),
            (
                3,
                node(
                    "table.make",
                    &[(
                        "columns",
                        "[(\"kind\", Name, [\"rock\", \"rock\", \"rock\", \"tree\"]), (\"weight\", Float, [1.0, 2.0, 1.0, 9.0]), (\"variant\", Name, [\"a\", \"b\", \"c\", \"t\"])]",
                    )],
                ),
            ),
            (
                4,
                node(
                    "attr.lookup",
                    &[
                        ("keys", "[(\"kind\", \"kind\")]"),
                        ("columns", "[\"variant\"]"),
                        ("multi", "weighted"),
                    ],
                ),
            ),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 4, "in"),
            (3, "out", 4, "table"),
            (4, "out", 5, "in"),
        ],
    );
    let p = out_points(&run(&g, empty_snap(), 6), "out");
    assert_eq!(p.len(), 10_000);
    assert!(
        !p.attrs.contains_key("weight"),
        "only the listed columns are copied"
    );
    let Some(regia_procgen::AttrColumn::Name(v)) = p.attrs.get("variant") else {
        panic!("variant column");
    };
    let count = |x: &str| v.iter().filter(|s| *s == x).count();
    assert_eq!(count("t"), 0, "the tree row never matches a rock");
    for (name, expect) in [("a", 2500), ("b", 5000), ("c", 2500)] {
        let n = count(name);
        assert!((expect - 300..=expect + 300).contains(&n), "{name}: {n}");
    }
}

#[test]
fn compare_logic_and_points_from_table() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(4.0, 1.0, 1.0)")])),
            (
                2,
                node(
                    "attr.compare",
                    &[
                        ("op", "gt"),
                        ("a", "\"position:x\""),
                        ("b", "0.0"),
                        ("target", "\"right\""),
                    ],
                ),
            ),
            (
                3,
                node(
                    "attr.logic",
                    &[("op", "not"), ("a", "\"right\""), ("target", "\"left\"")],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let p = out_points(&run(&g, empty_snap(), 1), "out");
    assert_eq!(
        p.attrs["right"],
        regia_procgen::AttrColumn::Bool(vec![false, false, true, true].into())
    );
    assert_eq!(
        p.attrs["left"],
        regia_procgen::AttrColumn::Bool(vec![true, true, false, false].into())
    );

    let t = graph(
        vec![
            (
                1,
                node(
                    "table.make",
                    &[(
                        "columns",
                        "[(\"at\", Vec3, [(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)]), (\"hp\", Int, [7, 8])]",
                    )],
                ),
            ),
            (2, node("points.from_table", &[("position", "\"at\"")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let p = out_points(&run(&t, empty_snap(), 1), "out");
    assert_eq!(p.position, vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
    assert_eq!(p.attrs["hp"], regia_procgen::AttrColumn::Int(vec![7, 8].into()));
}

#[test]
fn more_nodes_union_partition_match_volume_dedupe_nearest_repeat_enclose_pack_string() {
    let snap = empty_snap;
    // region.union: two overlapping single points; each takes the other's region value.
    let g = graph(
        vec![
            (1, node("gen.points", &[("points", "[((0.0, 0.0, 0.0))]")])),
            (2, node("gen.points", &[("points", "[((0.2, 0.0, 0.0))]")])),
            (
                3,
                node("attr.set", &[("target", "\"density\""), ("value", "0.25")]),
            ),
            (4, node("region.union", &[])),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 3, "in"),
            (3, "out", 4, "in"),
            (2, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let p = out_points(&run(&g, snap(), 1), "out");
    assert_eq!(p.len(), 2);
    assert_eq!(
        p.density[0], 1.0,
        "the low point sits inside the other's box (density 1)"
    );

    // items.partition by a name attribute, flow.match on the key.
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(4.0, 1.0, 1.0)")])),
            (
                2,
                node(
                    "attr.math",
                    &[("op", "sign"), ("a", "\"position:x\""), ("target", "\"side\"")],
                ),
            ),
            (
                3,
                node(
                    "attr.string",
                    &[
                        ("op", "format"),
                        ("parts", "[\"side\"]"),
                        ("pattern", "\"s{0}\""),
                        ("target", "\"key\""),
                    ],
                ),
            ),
            (4, node("items.partition", &[("by", "[\"key\"]")])),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let r = run(&g, snap(), 1);
    let items = &r.output.graph_outputs["out"].0;
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].scope["key"],
        regia_procgen::AttrValue::Name("s-1".into())
    );
    assert_eq!(items[1].scope["key"], regia_procgen::AttrValue::Name("s1".into()));

    // sample.volume over the generator box (half 10): 2 m cells → 10³ points.
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (2, node("sample.volume", &[])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in")],
    );
    assert_eq!(out_points(&run(&g, snap(), 1), "out").len(), 1000);

    // points.dedupe, points.nearest, points.repeat, points.enclose, attr.pack/unpack.
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.points",
                    &[(
                        "points",
                        "[((0.0, 0.0, 0.0)), ((0.05, 0.0, 0.0)), ((3.0, 0.0, 0.0))]",
                    )],
                ),
            ),
            (2, node("points.dedupe", &[("radius_m", "0.1")])),
            (3, node("points.nearest", &[])),
            (
                4,
                node("points.repeat", &[("count", "2"), ("offset", "(1.0, 0.0, 0.0)")]),
            ),
            (
                5,
                node(
                    "attr.pack",
                    &[
                        ("parts", "[\"position:x\", \"nearest_dist\"]"),
                        ("target", "\"pk\""),
                    ],
                ),
            ),
            (6, node("attr.unpack", &[("from", "\"pk\""), ("prefix", "\"u\"")])),
            (7, node("graph.output", &[])),
            (8, node("points.enclose", &[])),
            (9, node("graph.output", &[("pin", "\"box\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (2, "out", 3, "to"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
            (5, "out", 6, "in"),
            (6, "out", 7, "in"),
            (6, "out", 8, "in"),
            (8, "out", 9, "in"),
        ],
    );
    let r = run(&g, snap(), 1);
    let p = out_points(&r, "out");
    assert_eq!(p.len(), 6, "2 points after dedupe, each repeated twice");
    let xs: Vec<f32> = p.position.iter().map(|q| q[0]).collect();
    assert_eq!(xs, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    let Some(regia_procgen::AttrColumn::Float(d)) = p.attrs.get("u_y") else {
        panic!("unpacked nearest distance");
    };
    assert!(
        (d[0] - 3.0).abs() < 1e-6,
        "nearest other point is 3 m away: {d:?}"
    );
    let b = out_points(&r, "box");
    assert_eq!(b.len(), 1);
    assert!(
        (b.bounds_max[0][0] - b.bounds_min[0][0] - 6.0).abs() < 1e-5,
        "encloses x in [-0.5, 5.5]"
    );

    // flow.match routes by an item-scope key; untaken cases are inactive.
    let g = graph(
        vec![
            (1, node("gen.one", &[])),
            (
                2,
                node("attr.set", &[("target", "\"item.kind\""), ("value", "\"tree\"")]),
            ),
            (
                3,
                node(
                    "flow.match",
                    &[("key", "\"item.kind\""), ("cases", "[\"rock\", \"tree\"]")],
                ),
            ),
            (4, node("graph.output", &[("pin", "\"rock\"")])),
            (5, node("graph.output", &[("pin", "\"tree\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "case_0", 4, "in"),
            (3, "case_1", 5, "in"),
        ],
    );
    let r = run(&g, snap(), 1);
    assert!(r.report.node(4).expect("line").inactive);
    assert_eq!(out_points(&r, "tree").len(), 1);
}

#[test]
fn align_project_split_neighbors_rotation() {
    let map = regia_terrain::TerrainMap::from_heights(
        65,
        65,
        (0..65 * 65).map(|i| (i % 65) as f32 * 0.5).collect(),
        64.0,
        1.0,
        0.0,
    )
    .expect("map");
    let snap = ground_snapshot(&map, 32.0);
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(3.0, 1.0, 3.0)"), ("step_m", "(4.0, 1.0, 4.0)")],
                ),
            ),
            (2, node("in.ground", &[])),
            (3, node("points.project", &[("rotation", "true")])),
            (4, node("points.neighbors", &[("radius_m", "4.5")])),
            (5, node("points.split", &[("axis", "y")])),
            (6, node("graph.output", &[("pin", "\"low\"")])),
            (
                7,
                node(
                    "attr.rotation",
                    &[
                        ("op", "from_euler"),
                        ("a", "\"nb_center\""),
                        ("target", "\"spin\""),
                    ],
                ),
            ),
            (8, node("points.align", &[("to", "up")])),
            (9, node("graph.output", &[])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "onto"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
            (5, "low", 6, "in"),
            (4, "out", 7, "in"),
            (7, "out", 8, "in"),
            (8, "out", 9, "in"),
        ],
    );
    let r = run(&g, snap, 1);
    let p = out_points(&r, "out");
    assert_eq!(p.len(), 9);
    for (i, q) in p.position.iter().enumerate() {
        assert!(
            (q[1] - map.sample_bilinear(q[0], q[2])).abs() < 1e-5,
            "projected {i}"
        );
        let up = regia_procgen::data::quat(p.rotation[i]) * regia_core::math::Vec3::Y;
        assert!((up.y - 1.0).abs() < 1e-5, "aligned back to world up: {up:?}");
    }
    let Some(regia_procgen::AttrColumn::Float(nb)) = p.attrs.get("nb_count") else {
        panic!("nb_count");
    };
    assert_eq!(nb[4], 4.0, "the center point has 4 neighbors within 4.5 m");
    assert!(p.attrs.contains_key("spin"));
    let low = out_points(&r, "low");
    assert!(
        low.bounds_max.iter().all(|b| b[1] == 0.0),
        "low halves end at the box middle"
    );
}

#[test]
fn mask_in_filter_and_out() {
    use std::sync::Arc;
    let mut snap = (*empty_snap()).clone();
    // 2 x 1 mask: left half 0, right half 1 (red channel), over x in [-10, 10].
    snap.masks.insert(
        "masks/m.png".into(),
        regia_procgen::exec::MaskImage {
            width: 2,
            height: 1,
            rgba: vec![[0.0, 0.0, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0]],
        },
    );
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(8.0, 1.0, 1.0)"), ("step_m", "(2.0, 1.0, 1.0)")],
                ),
            ),
            (
                2,
                node(
                    "in.mask",
                    &[
                        ("asset", "\"masks/m.png\""),
                        ("rect", "[-10.0, -1.0, 10.0, 1.0]"),
                        ("filter", "nearest"),
                    ],
                ),
            ),
            (3, node("filter.region", &[("min_value", "0.5")])),
            (4, node("graph.output", &[])),
            (5, node("out.mask", &[("name", "\"grass\"")])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "region"),
            (3, "inside", 4, "in"),
            (2, "out", 5, "in"),
        ],
    );
    let r = run(&g, Arc::new(snap), 1);
    let p = out_points(&r, "out");
    assert!(
        p.position.iter().all(|q| q[0] > 0.0) && p.len() == 4,
        "only the right half: {:?}",
        p.position
    );
    assert_eq!(r.output.masks["grass"].sample(5.0, 0.0), 1.0);
    assert_eq!(r.output.masks["grass"].sample(-5.0, 0.0), 0.0);
}

#[test]
fn mesh_surface_sampling_is_area_weighted_and_on_the_surface() {
    use std::sync::Arc;
    let mut snap = (*empty_snap()).clone();
    // A 2 m x 1 m quad in the XZ plane (two triangles), area 2 m².
    snap.meshes.insert(
        "quad.glb".into(),
        Arc::new(regia_procgen::data::MeshData {
            asset: "quad.glb".into(),
            positions: vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [2.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            normals: Vec::new(),
            indices: vec![0, 2, 1, 0, 3, 2],
        }),
    );
    let snap = Arc::new(snap);
    let g = graph(
        vec![
            (1, node("in.mesh", &[("asset", "\"quad.glb\"")])),
            (2, node("sample.mesh_surface", &[("per_m2", "500.0")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let p = out_points(&run(&g, snap.clone(), 1), "out");
    assert_eq!(p.len(), 1000, "2 m² at 500 per m²");
    assert!(
        p.position
            .iter()
            .all(|q| q[1] == 0.0 && (0.0..=2.0).contains(&q[0]) && (0.0..=1.0).contains(&q[2]))
    );
    let left = p.position.iter().filter(|q| q[0] < 1.0).count();
    assert!(
        (420..=580).contains(&left),
        "uniform over area: {left} of 1000 in the left half"
    );
    // Placed by points: spawn.mesh writes `mesh`, the samples follow each point's transform.
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.points",
                    &[("points", "[((10.0, 0.0, 0.0)), ((20.0, 5.0, 0.0))]")],
                ),
            ),
            (2, node("spawn.mesh", &[("entries", "[(mesh: \"quad.glb\")]")])),
            (3, node("sample.mesh_surface", &[("per_m2", "10.0")])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let p = out_points(&run(&g, snap, 1), "out");
    assert_eq!(p.len(), 40);
    let high = p.position.iter().filter(|q| (q[1] - 5.0).abs() < 1e-4).count();
    assert_eq!(high, 20, "the second placement sits at y = 5");
}

#[test]
fn transform_apply_compose_invert() {
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.points",
                    &[("points", "[((1.0, 2.0, 3.0), (0.0, 90.0, 0.0), (2.0, 2.0, 2.0))]")],
                ),
            ),
            (
                2,
                node("attr.set", &[("target", "\"v\""), ("value", "(1.0, 0.0, 0.0)")]),
            ),
            (
                3,
                node(
                    "attr.transform",
                    &[("op", "apply_point"), ("b", "\"v\""), ("target", "\"world_v\"")],
                ),
            ),
            (
                4,
                node("attr.transform", &[("op", "invert"), ("target", "\"inv\"")]),
            ),
            (
                5,
                node(
                    "attr.transform",
                    &[("op", "compose"), ("b", "\"inv\""), ("target", "\"id\"")],
                ),
            ),
            (6, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
            (5, "out", 6, "in"),
        ],
    );
    let p = out_points(&run(&g, empty_snap(), 1), "out");
    let Some(regia_procgen::AttrColumn::Vec3(w)) = p.attrs.get("world_v") else {
        panic!("world_v");
    };
    // Yaw 90 degrees turns +X into -Z; scale 2; then translate.
    let expect = [1.0, 2.0, 1.0];
    assert!(
        w[0].iter().zip(expect).all(|(a, b)| (a - b).abs() < 1e-5),
        "{:?}",
        w[0]
    );
    let Some(regia_procgen::AttrColumn::Transform(id)) = p.attrs.get("id") else {
        panic!("id");
    };
    assert!(
        id[0].t.iter().all(|c| c.abs() < 1e-5) && id[0].s.iter().all(|c| (c - 1.0).abs() < 1e-5),
        "{:?}",
        id[0]
    );
}

/// `points.flat_only` fits the five probe samples on the stack (audit finding 12).
#[test]
fn flat_only_does_not_allocate_per_point() {
    let map =
        regia_terrain::TerrainMap::from_heights(65, 65, vec![1.0; 65 * 65], 64.0, 1.0, 0.0).expect("map");
    let snap = ground_snapshot(&map, 32.0);
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(10.0, 1.0, 20.0)")])),
            (2, node("in.ground", &[])),
            (3, node("points.flat_only", &[])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "ground"), (3, "out", 4, "in")],
    );
    regia_procgen::probe::arm();
    let n = out_points(&run(&g, snap, 1), "out").len();
    assert!(n > 1, "the flat grid should keep its points, kept {n}");
    assert_eq!(
        regia_procgen::probe::flat_vecs(),
        0,
        "flat_only built a Vec for each of {n} points"
    );
}
