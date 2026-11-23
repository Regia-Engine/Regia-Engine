// SPDX-License-Identifier: MPL-2.0
//! Spec 68 conformance tests (lane spec-28-68, 2026-11-10): behavior the spec names that the § 10
//! tests in `plants.rs` do not cover: flowering, `max_order`, opposite and whorled buds, the per-step
//! run report, the `growth_steps` limit, what the shaping nodes do (not only that they keep a
//! valid tree), and the `plant.leaves` mask and per-branch cap. Pipeline feature, no GPU.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, node};
use regia_procgen::data::{Bundle, DataItem, Payload};
use regia_procgen::exec::{Frame, StaticSnapshot};
use regia_procgen::nodes::Domain;
use regia_procgen::plant::{BudState, Plant, PlantMesh, PlantNode, PlantSkeleton, SlotKind};
use regia_procgen::{
    CancelToken, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs, RunResult,
};

fn plant_graph(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> ProcGraph {
    let mut g = ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges
            .iter()
            .map(|(a, ap, b, bp)| Edge::new(*a, ap, *b, bp))
            .collect(),
        ..ProcGraph::default()
    };
    g.flags.domain = Domain::Plant;
    g
}

fn run_frame(g: &ProcGraph, seed: u32, frame_inputs: BTreeMap<String, Bundle>) -> RunResult {
    let c = match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), seed);
    let frame = Frame {
        graph_inputs: frame_inputs,
        ..Frame::default()
    };
    match Evaluator::new().run_frame(&c, &inputs, &CancelToken::new(), &frame) {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

fn run(g: &ProcGraph, seed: u32) -> RunResult {
    run_frame(g, seed, BTreeMap::new())
}

fn skeleton(r: &RunResult) -> PlantSkeleton {
    match r
        .output
        .graph_outputs
        .get("out")
        .and_then(|b| b.0.first())
        .map(|i| &i.payload)
    {
        Some(Payload::PlantSkeleton(s)) => (**s).clone(),
        Some(Payload::PlantMesh(m)) => (*m.skeleton).clone(),
        _ => panic!("no skeleton on `out`"),
    }
}

fn mesh(r: &RunResult) -> PlantMesh {
    match r
        .output
        .graph_outputs
        .get("out")
        .and_then(|b| b.0.first())
        .map(|i| &i.payload)
    {
        Some(Payload::PlantMesh(m)) => (**m).clone(),
        _ => panic!("no mesh on `out`"),
    }
}

/// seed → grow (settings) → output `out`.
fn grow_graph(grow: &[(&str, &str)]) -> ProcGraph {
    plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", grow)),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

/// seed → grow (6 steps) → radius → `ty` (settings) → output `out`.
fn shaped(ty: &str, settings: &[(&str, &str)]) -> Plant {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "6")])),
            (3, node("plant.radius", &[])),
            (4, node(ty, settings)),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    skeleton(&run(&g, 7)).plants.remove(0)
}

/// The same tree without the shaping node (seed → grow → radius).
fn unshaped() -> Plant {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "6")])),
            (3, node("plant.radius", &[])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    skeleton(&run(&g, 7)).plants.remove(0)
}

/// Trunk node indices, root first (each the order-0 child of the previous).
fn trunk(p: &Plant) -> Vec<usize> {
    let children = p.children();
    let mut out = vec![0usize];
    while let Some(&c) = children[*out.last().unwrap_or(&0)]
        .iter()
        .find(|&&c| p.nodes[c as usize].order == 0)
    {
        out.push(c as usize);
    }
    out
}

fn yaw_deg(d: [f64; 3]) -> f64 {
    regia_procgen::detmath::atan2(d[0], d[2]).to_degrees()
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

// § 4 Flowering: terminal buds that become `Bloom` stop growing and their nodes carry `bloom`.
#[test]
fn bloom_buds_stop_growing_and_mark_their_nodes() {
    let plain = skeleton(&run(&grow_graph(&[("growth_steps", "6")]), 7));
    assert!(
        plain.plants[0]
            .nodes
            .iter()
            .all(|n| n.bud != BudState::Bloom && !n.bloom),
        "bloom_chance 0 (the default) never blooms"
    );
    let bloomed = skeleton(&run(
        &grow_graph(&[
            ("growth_steps", "6"),
            ("bloom_after_steps", "2"),
            ("bloom_chance", "1.0"),
        ]),
        7,
    ));
    let p = &bloomed.plants[0];
    let blooms: Vec<usize> = (0..p.nodes.len())
        .filter(|&i| p.nodes[i].bud == BudState::Bloom)
        .collect();
    assert!(
        !blooms.is_empty(),
        "bloom_chance 1 after step 2 blooms terminal buds"
    );
    let children = p.children();
    for &i in &blooms {
        assert!(p.nodes[i].bloom, "node {i}: a Bloom bud marks its node");
        assert!(
            children[i]
                .iter()
                .all(|&c| p.nodes[c as usize].order != p.nodes[i].order),
            "node {i}: a Bloom terminal bud does not continue its axis"
        );
    }
    assert!(
        p.nodes.len() < plain.plants[0].nodes.len(),
        "blooming stops growth: {} vs {} nodes",
        p.nodes.len(),
        plain.plants[0].nodes.len()
    );
}

// § 4 step 5: buds above `max_order` are not created.
#[test]
fn max_order_caps_nodes_and_buds() {
    for max in [0u32, 1, 2] {
        let sk = skeleton(&run(
            &grow_graph(&[("growth_steps", "8"), ("max_order", &max.to_string())]),
            7,
        ));
        let p = &sk.plants[0];
        assert!(p.nodes.len() > 3, "max_order {max}: the tree grows");
        assert!(
            p.nodes.iter().all(|n| n.order <= max),
            "max_order {max}: node orders {:?}",
            p.max_order()
        );
        assert!(
            p.buds.iter().all(|b| b.order <= max),
            "max_order {max}: no bud above the cap"
        );
        if max > 0 {
            assert_eq!(p.max_order(), max, "max_order {max} is reached");
        }
    }
}

/// Lateral bud yaws per trunk node of a one-step straight tree.
fn trunk_bud_yaws(buds_per_node: u32, divergence: f64) -> Vec<Vec<f64>> {
    let sk = skeleton(&run(
        &grow_graph(&[
            ("growth_steps", "1"),
            ("buds_per_node", &buds_per_node.to_string()),
            ("divergence_deg", &format!("{divergence:?}")),
            ("wander_deg", "0.0"),
            ("light_pull", "0.0"),
            ("up_pull", "0.0"),
        ]),
        7,
    ));
    let p = &sk.plants[0];
    let mut by_node: BTreeMap<u32, Vec<f64>> = BTreeMap::new();
    for b in p.buds.iter().filter(|b| p.nodes[b.node as usize].order == 0) {
        by_node.entry(b.node).or_default().push(yaw_deg(b.dir));
    }
    by_node.into_values().collect()
}

// § 4 shoot structure: 2 buds per node with 90° divergence is decussate; n buds are a whorl.
#[test]
fn opposite_buds_with_90_degree_divergence_are_decussate_and_whorls_split_evenly() {
    let nodes = trunk_bud_yaws(2, 90.0);
    assert!(nodes.len() >= 2, "{} trunk nodes with buds", nodes.len());
    for yaws in &nodes {
        assert_eq!(yaws.len(), 2, "two buds per node");
        let d = (yaws[1] - yaws[0]).rem_euclid(360.0);
        assert!((d - 180.0).abs() < 0.01, "opposite pair {d}");
    }
    for w in nodes.windows(2) {
        let d = (w[1][0] - w[0][0]).rem_euclid(360.0);
        assert!((d - 90.0).abs() < 0.01, "successive pairs turn by {d}");
    }
    for yaws in trunk_bud_yaws(5, 0.0) {
        assert_eq!(yaws.len(), 5, "a whorl of five");
        for w in yaws.windows(2) {
            let d = (w[1] - w[0]).rem_euclid(360.0);
            assert!((d - 72.0).abs() < 0.01, "whorl spacing {d}");
        }
    }
}

// § 4: the run report lists per step the node count, active buds and shed branches.
#[test]
fn run_report_lists_nodes_active_buds_and_shed_branches_per_step() {
    let steps = 8u32;
    let r = run(
        &grow_graph(&[("growth_steps", &steps.to_string()), ("shed_below_light", "0.5")]),
        7,
    );
    let grow = r
        .report
        .nodes
        .iter()
        .find(|n| n.ty == "plant.grow")
        .expect("the grow node reports");
    let sk = skeleton(&r);
    let mut shed = 0u64;
    for step in 1..=steps {
        for what in ["nodes", "active_buds", "shed"] {
            assert!(
                grow.counters.contains_key(&format!("step_{step:02}_{what}")),
                "step {step}: `{what}` counter in {:?}",
                grow.counters.keys().collect::<Vec<_>>()
            );
        }
        shed += grow.counters[&format!("step_{step:02}_shed")];
    }
    assert_eq!(
        grow.counters[&format!("step_{steps:02}_nodes")],
        sk.node_count() as u64,
        "the last step's node count is the skeleton's"
    );
    assert_eq!(
        shed,
        u64::from(sk.plants[0].shed_branches),
        "per-step shed counts add up to the plant's shed branches"
    );
    assert!(shed > 0, "shed_below_light 0.5 sheds branches");
}

// § 4 validation: `growth_steps` ≤ 64.
#[test]
fn growth_steps_over_64_is_refused_at_compile() {
    let ok = grow_graph(&[("growth_steps", "64")]);
    assert!(ok.compile(&ProcNodeRegistry::with_builtins()).is_ok());
    let e = grow_graph(&[("growth_steps", "65")])
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("65 growth steps");
    assert!(e.has("setting_out_of_range"), "{:?}", e.0);
}

// § 5 `plant.bend`: branch segments sag downward; the trunk does not move.
#[test]
fn bend_sags_branches_and_keeps_the_trunk() {
    let before = unshaped();
    let after = shaped("plant.bend", &[("load_per_m", "50.0"), ("max_deg", "35.0")]);
    assert_eq!(
        before.nodes.len(),
        after.nodes.len(),
        "bend moves, never adds or removes"
    );
    for i in trunk(&before) {
        assert!(
            dist(before.nodes[i].position, after.nodes[i].position) < 1e-9,
            "trunk node {i} stays"
        );
    }
    let lateral_y = |p: &Plant| -> f64 {
        p.nodes
            .iter()
            .filter(|n| n.order >= 1)
            .map(|n| n.position[1])
            .sum()
    };
    assert!(
        lateral_y(&after) < lateral_y(&before) - 1e-3,
        "branches sag: {} -> {}",
        lateral_y(&before),
        lateral_y(&after)
    );
    // Segment lengths are kept (rotations only).
    for (i, n) in after.nodes.iter().enumerate() {
        if let Some(par) = n.parent {
            let a = dist(n.position, after.nodes[par as usize].position);
            let b = dist(before.nodes[i].position, before.nodes[par as usize].position);
            assert!((a - b).abs() < 1e-9, "segment {i} length {b} -> {a}");
        }
    }
}

// § 5 `plant.lean`: the base leans toward `dir` and the trunk is upright again above
// `recover_height_m` (subtrees rotate rigidly).
#[test]
fn lean_tilts_the_base_and_recovers_with_height() {
    let before = unshaped();
    let after = shaped(
        "plant.lean",
        &[
            ("deg", "20.0"),
            ("recover_height_m", "1.0"),
            ("dir", "(1.0, 0.0, 0.0)"),
        ],
    );
    let t = trunk(&before);
    assert!(t.len() >= 6, "a trunk of {} nodes", t.len());
    assert!(before.height() > 2.0, "the trunk rises above the recover height");
    let top = *t.last().unwrap_or(&0);
    let root = before.nodes[0].position;
    assert!(
        after.nodes[top].position[0] - root[0] > before.nodes[top].position[0] - root[0] + 0.05,
        "the trunk top moves toward +x"
    );
    // The first trunk segment leans toward +x.
    let seg = |p: &Plant, a: usize, b: usize| {
        let (pa, pb) = (p.nodes[a].position, p.nodes[b].position);
        let d = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        [d[0] / l, d[1] / l, d[2] / l]
    };
    assert!(seg(&after, t[0], t[1])[0] > seg(&before, t[0], t[1])[0] + 0.1);
    // The top segment is far above 1 m: its direction is the unleaned one again.
    let (a, b) = (t[t.len() - 2], t[t.len() - 1]);
    let (s0, s1) = (seg(&before, a, b), seg(&after, a, b));
    assert!(
        dist(s0, s1) < 1e-6,
        "top segment direction {s0:?} -> {s1:?} (recovered)"
    );
}

// § 5 `plant.twist`: lateral branches roll about their parent axis; the trunk stays.
#[test]
fn twist_rolls_lateral_branches_about_their_parent() {
    let before = unshaped();
    let after = shaped("plant.twist", &[("deg_per_node", "30.0")]);
    for i in trunk(&before) {
        assert!(dist(before.nodes[i].position, after.nodes[i].position) < 1e-9);
    }
    let moved = (0..before.nodes.len())
        .filter(|&i| dist(before.nodes[i].position, after.nodes[i].position) > 1e-6)
        .count();
    assert!(moved > 0, "lateral branches roll");
    // A roll about the parent axis keeps each node's distance to its parent.
    for (i, n) in after.nodes.iter().enumerate() {
        if let Some(par) = n.parent {
            let a = dist(n.position, after.nodes[par as usize].position);
            let b = dist(before.nodes[i].position, before.nodes[par as usize].position);
            assert!((a - b).abs() < 1e-9, "segment {i}");
        }
    }
    let none = shaped("plant.twist", &[("deg_per_node", "0.0")]);
    assert_eq!(none.nodes.len(), before.nodes.len());
    assert!(
        (0..none.nodes.len()).all(|i| dist(none.nodes[i].position, before.nodes[i].position) == 0.0),
        "deg_per_node 0 (the default) is a no-op"
    );
}

// § 5 `plant.cut by: order` and `by: radius` remove whole branches.
#[test]
fn cut_by_order_and_radius_removes_whole_branches() {
    let before = unshaped();
    let by_order = shaped("plant.cut", &[("by", "order"), ("max", "1.0")]);
    assert!(by_order.nodes.iter().all(|n| n.order <= 1));
    assert!(by_order.nodes.len() < before.nodes.len());
    assert_eq!(
        by_order.nodes.len(),
        before.nodes.iter().filter(|n| n.order <= 1).count(),
        "every node of order <= 1 survives (their parents are of lower order)"
    );
    let min_r = 0.006;
    let by_radius = shaped("plant.cut", &[("by", "radius"), ("min", &format!("{min_r:?}"))]);
    assert!(by_radius.nodes.len() < before.nodes.len());
    assert!(
        by_radius.nodes.iter().skip(1).all(|n| n.radius >= min_r),
        "no node thinner than {min_r} m is kept"
    );
}

/// seed → grow → radius → mesh → leaves (settings, optional mask input) → output `out`.
fn leaves(settings: &[(&str, &str)], mask: Option<Bundle>) -> PlantMesh {
    let mut nodes = vec![
        (1, node("plant.seed", &[])),
        (2, node("plant.grow", &[("growth_steps", "8")])),
        (3, node("plant.radius", &[])),
        (4, node("plant.mesh", &[])),
        (5, node("plant.leaves", settings)),
        (6, node("graph.output", &[])),
    ];
    let mut edges = vec![
        (1, "out", 2, "in"),
        (2, "out", 3, "in"),
        (3, "out", 4, "in"),
        (4, "out", 5, "in"),
        (5, "out", 6, "in"),
    ];
    let mut frame = BTreeMap::new();
    if let Some(m) = mask {
        nodes.push((7, node("graph.input", &[("pin", "\"mask\"")])));
        edges.push((7, "out", 5, "mask"));
        frame.insert("mask".to_string(), m);
    }
    mesh(&run_frame(&plant_graph(nodes, &edges), 7, frame))
}

// § 6.2 `mask`: nothing is placed inside the mask regions.
#[test]
fn leaves_mask_keeps_its_region_bare() {
    let free = leaves(&[], None);
    let root = free.skeleton.plants[0].nodes[0].position;
    let plus = |m: &PlantMesh| {
        m.leaf_sites
            .iter()
            .filter(|c| f64::from(c[0]) - root[0] > 0.15)
            .count()
    };
    assert!(plus(&free) > 10, "leaves on the +x side without a mask");
    // A box over everything at x > 0 (sites are branch points; a leaf center sits at most half a
    // leaf, 0.05 m, from its site).
    let mask = Bundle::one(DataItem::new(Payload::Shape(box_shape(
        [50.0 + root[0] as f32, 0.0, 0.0],
        [50.0, 100.0, 100.0],
        0.0,
    ))));
    let masked = leaves(&[], Some(mask));
    assert_eq!(plus(&masked), 0, "no leaf inside the mask");
    assert!(
        masked.leaf_sites.len() > 10 && masked.leaf_sites.len() < free.leaf_sites.len(),
        "the unmasked side keeps its leaves: {} of {}",
        masked.leaf_sites.len(),
        free.leaf_sites.len()
    );
}

// § 6.2 `per_branch_max`: at most that many sites per branch.
#[test]
fn leaves_per_branch_max_caps_each_branch() {
    let free = leaves(&[], None);
    let capped = leaves(&[("per_branch_max", "1")], None);
    let axes = capped.skeleton.plants[0].axes().len();
    assert!(
        capped.leaf_sites.len() <= axes,
        "{} leaves on {axes} branches",
        capped.leaf_sites.len()
    );
    assert!(capped.leaf_sites.len() < free.leaf_sites.len());
    assert!(!capped.leaf_sites.is_empty());
    let none = leaves(&[("per_branch_max", "0")], None);
    assert!(none.leaf_sites.is_empty(), "per_branch_max 0 places nothing");
}

/// seed → grow (6 steps) → radius → `plant.cut` (settings) with a shape on its `shape` pin.
fn cut_with_shape(settings: &[(&str, &str)], shape: Bundle) -> Plant {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "6")])),
            (3, node("plant.radius", &[])),
            (4, node("plant.cut", settings)),
            (5, node("graph.output", &[])),
            (6, node("graph.input", &[("pin", "\"cutter\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
            (6, "out", 4, "shape"),
        ],
    );
    skeleton(&run_frame(&g, 7, BTreeMap::from([("cutter".to_string(), shape)])))
        .plants
        .remove(0)
}

// § 5 `plant.cut by: distance` shortens crossing segments to the boundary; `by: shape` removes
// branches whose base lies outside `[min, max]` of the shape's value.
#[test]
fn cut_by_distance_and_shape() {
    let before = unshaped();
    let max_d = 1.5;
    let cut = shaped("plant.cut", &[("by", "distance"), ("max", &format!("{max_d:?}"))]);
    let root = cut.nodes[0].position;
    assert!(cut.nodes.len() < before.nodes.len());
    assert!(
        cut.nodes.iter().all(|n| dist(n.position, root) <= max_d + 1e-9),
        "nothing beyond {max_d} m of the root"
    );
    assert!(
        cut.nodes.iter().any(|n| dist(n.position, root) > max_d - 0.05),
        "a crossing segment is shortened to the boundary, not dropped"
    );
    // A box covering x < root.x: keep only nodes whose shape value is 0 (outside the box).
    let r = before.nodes[0].position;
    let half = Bundle::one(DataItem::new(Payload::Shape(box_shape(
        [r[0] as f32 - 50.0, 0.0, 0.0],
        [50.0 - 0.05, 100.0, 100.0],
        0.0,
    ))));
    let kept = cut_with_shape(&[("by", "shape"), ("max", "0.5")], half);
    assert!(kept.nodes.len() < before.nodes.len(), "the box side is cut");
    assert!(
        kept.nodes
            .iter()
            .skip(1)
            .all(|n| n.position[0] > r[0] - 0.05 - 1e-9),
        "no kept node inside the cutting box"
    );
    for (i, n) in kept.nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            assert!((p as usize) < i, "parents first");
        }
    }
}

// § 5 `plant.attach sites: along`: parts attach along branches at `per_m` sites per meter.
#[test]
fn attach_along_branches_by_density() {
    let attach = |per_m: &str| -> Plant {
        let g = plant_graph(
            vec![
                (1, node("plant.seed", &[])),
                (2, node("plant.grow", &[("growth_steps", "4")])),
                (3, node("plant.seed", &[])),
                (
                    4,
                    node("plant.grow", &[("growth_steps", "1"), ("internode_m", "0.05")]),
                ),
                (
                    5,
                    node(
                        "plant.attach",
                        &[("sites", "along"), ("per_m", per_m), ("scale_by_radius", "false")],
                    ),
                ),
                (6, node("graph.output", &[])),
            ],
            &[
                (1, "out", 2, "in"),
                (3, "out", 4, "in"),
                (2, "out", 5, "base"),
                (4, "out", 5, "parts"),
                (5, "out", 6, "in"),
            ],
        );
        skeleton(&run(&g, 7)).plants.remove(0)
    };
    let sparse = attach("4.0");
    let dense = attach("12.0");
    let base = skeleton(&run(&grow_graph(&[("growth_steps", "4")]), 7))
        .plants
        .remove(0);
    assert!(sparse.nodes.len() > base.nodes.len(), "parts attached");
    assert!(
        dense.nodes.len() > sparse.nodes.len(),
        "more sites per meter attach more parts: {} vs {}",
        dense.nodes.len(),
        sparse.nodes.len()
    );
    for p in [&sparse, &dense] {
        for (i, n) in p.nodes.iter().enumerate() {
            if let Some(par) = n.parent {
                assert!((par as usize) < i);
                assert!(
                    n.order >= p.nodes[par as usize].order,
                    "attached parts sit above their site's order"
                );
            }
        }
    }
}

/// Cards in the leaf slots as `(length axis, unit normal, length)`: a card's corners are
/// base − w, base + w, tip + w, tip − w, every corner carrying the face normal.
fn cards(m: &PlantMesh) -> Vec<([f64; 3], [f64; 3], f64)> {
    let mut out = Vec::new();
    for slot in m.slots.iter().filter(|s| s.kind == SlotKind::Leaf) {
        let (quads, rest) = slot.positions.as_chunks::<4>();
        assert!(rest.is_empty(), "card slots hold whole quads");
        for (q, quad) in quads.iter().enumerate() {
            let v = sub3(quad[3].map(f64::from), quad[0].map(f64::from));
            let l = dist(v, [0.0; 3]);
            out.push((v.map(|x| x / l), slot.normals[4 * q].map(f64::from), l));
        }
    }
    out
}

fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

// § 6.2 `orient`: `sun` lays cards flat facing up, `up` stands them on their length, `along` follows
// the branch, `random` scatters them; `orient_jitter_deg` bounds the tilt away from the mode.
#[test]
fn leaves_orient_modes_and_jitter() {
    let sun = cards(&leaves(&[("orient", "sun"), ("orient_jitter_deg", "0.0")], None));
    assert!(sun.len() > 10);
    for (axis, n, _) in &sun {
        assert!(axis[1].abs() < 1e-4, "sun: the card lies flat, axis {axis:?}");
        assert!(n[1] > 0.9999, "sun: the card faces up, normal {n:?}");
    }
    let up = cards(&leaves(&[("orient", "up"), ("orient_jitter_deg", "0.0")], None));
    assert_eq!(up.len(), sun.len(), "orientation does not change the sites");
    for (axis, n, _) in &up {
        assert!(axis[1] > 0.9999, "up: the card stands upright, axis {axis:?}");
        assert!(n[1].abs() < 1e-4, "up: the card faces sideways, normal {n:?}");
    }
    let m = leaves(&[("orient", "along"), ("orient_jitter_deg", "0.0")], None);
    let p = &m.skeleton.plants[0];
    let segs: Vec<[f64; 3]> = p
        .nodes
        .iter()
        .filter_map(|n| {
            let a = p.nodes[n.parent? as usize].position;
            let d = sub3(n.position, a);
            let l = dist(d, [0.0; 3]);
            (l > 0.0).then(|| d.map(|x| x / l))
        })
        .collect();
    let along = cards(&m);
    assert_eq!(along.len(), sun.len());
    for (axis, n, _) in &along {
        assert!(
            dot3(*axis, *n).abs() < 1e-4,
            "along: the face is perpendicular to the length"
        );
        assert!(
            segs.iter().any(|s| dot3(*s, *axis) > 0.9999),
            "along: the card follows a branch segment, axis {axis:?}"
        );
    }
    assert!(
        along.iter().any(|(a, _, _)| a[1].abs() > 0.1 && a[1] < 0.9999),
        "along differs from sun and up"
    );
    let random = cards(&leaves(
        &[("orient", "random"), ("orient_jitter_deg", "0.0")],
        None,
    ));
    assert!(
        random.iter().any(|(a, _, _)| a[1] < -0.3),
        "random: some cards hang down"
    );
    assert!(
        random.iter().any(|(a, _, _)| a[1] > 0.3),
        "random: some cards point up"
    );
    for (axis, n, _) in &random {
        assert!(dot3(*axis, *n).abs() < 1e-4);
    }
    // Jitter tilts each card by at most `orient_jitter_deg` away from the mode's axis.
    let jit = cards(&leaves(&[("orient", "up"), ("orient_jitter_deg", "30.0")], None));
    let tilt = |a: &[f64; 3]| regia_procgen::detmath::acos(a[1].clamp(-1.0, 1.0)).to_degrees();
    assert!(
        jit.iter().all(|(a, _, _)| tilt(a) <= 30.0 + 1e-3),
        "tilt within the jitter"
    );
    assert!(
        jit.iter().any(|(a, _, _)| tilt(a) > 1.0),
        "jitter tilts some cards"
    );
}

// § 6.2 `size_to_tip`: an empty curve keeps `size_m`; a curve scales each leaf by its distance from
// the branch tip (tips sit at distance 0); a curve that reaches 0 places no leaf.
#[test]
fn leaves_size_to_tip_scales_by_distance_from_the_tip() {
    let len_range = |m: &PlantMesh| {
        let c = cards(m);
        assert!(!c.is_empty());
        c.iter()
            .fold((f64::MAX, 0.0f64), |(lo, hi), (_, _, l)| (lo.min(*l), hi.max(*l)))
    };
    let (lo, hi) = len_range(&leaves(&[], None));
    assert!(
        lo >= 0.06 - 1e-4 && hi <= 0.1 + 1e-4,
        "default size_m [0.06, 0.1]: {lo}..{hi}"
    );
    let (lo, hi) = len_range(&leaves(&[("size_to_tip", "[(0.0, 2.0)]")], None));
    assert!(lo >= 0.12 - 1e-4 && hi <= 0.2 + 1e-4, "doubled: {lo}..{hi}");
    // Tips only: distance 0 reads the curve's first key (0.5), not its far end (2.0).
    let (lo, hi) = len_range(&leaves(
        &[("where", "tips"), ("size_to_tip", "[(0.0, 0.5), (1.0, 2.0)]")],
        None,
    ));
    assert!(
        lo >= 0.03 - 1e-4 && hi <= 0.05 + 1e-4,
        "tips at distance 0: {lo}..{hi}"
    );
    let none = leaves(&[("size_to_tip", "[(0.0, 0.0)]")], None);
    assert!(
        cards(&none).is_empty() && none.leaf_sites.is_empty(),
        "size 0 places nothing"
    );
}

/// A hand-made L-shaped skeleton: a 1 m trunk up +y, then a 2-node order-1 branch along +x; the
/// `attach` column marks the sites.
fn l_plant(attach: [f64; 4]) -> Plant {
    let mut nodes = vec![PlantNode::root([0.0; 3], [0.0, 1.0, 0.0], 0.1, 0.0)];
    for (pos, parent, order, dir) in [
        ([0.0, 1.0, 0.0], 0, 0, [0.0, 1.0, 0.0]),
        ([1.0, 1.0, 0.0], 1, 1, [1.0, 0.0, 0.0]),
        ([2.0, 1.0, 0.0], 2, 1, [1.0, 0.0, 0.0]),
    ] {
        let mut n = nodes[0].clone();
        n.position = pos;
        n.parent = Some(parent);
        n.order = order;
        n.dir = dir;
        n.radius = 0.05;
        nodes.push(n);
    }
    Plant {
        seed: 3,
        nodes,
        attrs: BTreeMap::from([("attach".to_string(), attach.to_vec())]),
        ..Plant::default()
    }
}

/// A straight part of `n` nodes, 0.5 m per segment up +y.
fn chain_part(n: usize) -> Plant {
    let mut nodes = vec![PlantNode::root([0.0; 3], [0.0, 1.0, 0.0], 0.05, 0.0)];
    for i in 1..n {
        let mut c = nodes[0].clone();
        c.position = [0.0, 0.5 * i as f64, 0.0];
        c.parent = Some(i as u32 - 1);
        nodes.push(c);
    }
    Plant {
        seed: 5,
        nodes,
        ..Plant::default()
    }
}

fn skel_bundle(p: Plant) -> Bundle {
    Bundle::one(DataItem::new(Payload::PlantSkeleton(Arc::new(PlantSkeleton {
        plants: vec![p],
    }))))
}

/// `plant.attach` (settings) with hand-made `base` and `parts` skeletons on graph inputs.
fn attach_inputs(
    settings: &[(&str, &str)],
    base: Plant,
    part: Plant,
) -> Result<RunResult, regia_procgen::RunError> {
    let g = plant_graph(
        vec![
            (1, node("graph.input", &[("pin", "\"base\"")])),
            (2, node("graph.input", &[("pin", "\"parts\"")])),
            (3, node("plant.attach", settings)),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "base"), (2, "out", 3, "parts"), (3, "out", 4, "in")],
    );
    let c = match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let frame = Frame {
        graph_inputs: BTreeMap::from([
            ("base".to_string(), skel_bundle(base)),
            ("parts".to_string(), skel_bundle(part)),
        ]),
        ..Frame::default()
    };
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 7);
    Evaluator::new().run_frame(&c, &inputs, &CancelToken::new(), &frame)
}

// § 5 `plant.attach sites: attr`: a node is a site when its custom attribute is above 0.5; `align:
// along` turns the part onto the branch direction, `align: up` keeps it upright.
#[test]
fn attach_attr_sites_and_align_up() {
    let run_attach = |align: &str, attach: [f64; 4]| -> Plant {
        match attach_inputs(
            &[("sites", "attr"), ("align", align), ("scale_by_radius", "false")],
            l_plant(attach),
            chain_part(2),
        ) {
            Ok(r) => skeleton(&r).plants.remove(0),
            Err(e) => panic!("run failed: {e}"),
        }
    };
    // 0.5 is not above 0.5: only node 2 is a site.
    let along = run_attach("along", [0.0, 0.5, 1.0, 0.0]);
    assert_eq!(along.nodes.len(), 5, "one site, one part node");
    let new = &along.nodes[4];
    assert_eq!(new.parent, Some(2), "the part hangs off the marked node");
    assert_eq!(new.order, 2, "one order above the site");
    assert_eq!(
        along.attrs["attach"].len(),
        5,
        "custom columns stay as long as the nodes"
    );
    assert_eq!(along.attrs["attach"][4], 0.0);
    let off = sub3(new.position, [1.0, 1.0, 0.0]);
    assert!(
        dist(off, [0.5, 0.0, 0.0]) < 1e-9,
        "along: the part follows +x, offset {off:?}"
    );
    assert!(new.dir[0] > 0.9999, "along: direction {:?}", new.dir);
    let up = run_attach("up", [0.0, 0.5, 1.0, 0.0]);
    let off = sub3(up.nodes[4].position, [1.0, 1.0, 0.0]);
    assert!(
        dist(off, [0.0, 0.5, 0.0]) < 1e-9,
        "up: the part stays upright, offset {off:?}"
    );
    assert!(up.nodes[4].dir[1] > 0.9999);
    // Two marked nodes give two parts; no marked node leaves the base as it was.
    assert_eq!(run_attach("along", [0.0, 1.0, 1.0, 0.0]).nodes.len(), 6);
    assert_eq!(run_attach("along", [0.0; 4]), l_plant([0.0; 4]));
}

// § 4 validation: a plant holds at most 200 000 nodes; growing past it (here through `plant.attach`)
// fails with `limit_exceeded`, and a plant just under the limit is fine.
#[test]
fn plant_node_limit_is_200000() {
    // A base chain of `sites` nodes, every one an attach site, with a 1000-node part on each: the
    // part's root merges into the site, so each site adds 999 nodes.
    let base = |sites: usize| {
        let mut p = chain_part(sites);
        p.attrs.insert("attach".to_string(), vec![1.0; sites]);
        p
    };
    let settings = [("sites", "attr"), ("scale_by_radius", "false")];
    let under = match attach_inputs(&settings, base(199), chain_part(1000)) {
        Ok(r) => skeleton(&r).plants.remove(0),
        Err(e) => panic!("199 000 nodes is under the limit: {e}"),
    };
    assert_eq!(under.nodes.len(), 199 + 199 * 999);
    match attach_inputs(&settings, base(201), chain_part(1000)) {
        Err(regia_procgen::RunError::Node { code, message, .. }) => {
            assert_eq!(code, "limit_exceeded", "{message}");
            assert!(message.contains("200000"), "{message}");
        }
        Err(e) => panic!("expected limit_exceeded, got {e}"),
        Ok(r) => panic!("{} nodes passed the limit", skeleton(&r).node_count()),
    }
}
