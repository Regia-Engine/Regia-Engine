// SPDX-License-Identifier: MPL-2.0
//! Spec 68 § 10 plant tests (pipeline feature, no GPU). Recorded hashes were recorded on the
//! the build server Linux x86-64 reference (2026-10-29).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, hex, node};
use regia_procgen::data::{Bundle, DataItem, Payload};
use regia_procgen::exec::{Frame, StaticSnapshot};
use regia_procgen::nodes::Domain;
use regia_procgen::plant::{BudState, PlantMesh, PlantSkeleton, SlotKind};
use regia_procgen::{
    CancelToken, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs, RunResult,
};

/// A plant-domain graph.
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

fn skeleton(r: &RunResult, pin: &str) -> PlantSkeleton {
    match r
        .output
        .graph_outputs
        .get(pin)
        .and_then(|b| b.0.first())
        .map(|i| &i.payload)
    {
        Some(Payload::PlantSkeleton(s)) => (**s).clone(),
        Some(Payload::PlantMesh(m)) => (*m.skeleton).clone(),
        _ => panic!("no skeleton on `{pin}`"),
    }
}

fn mesh(r: &RunResult, pin: &str) -> PlantMesh {
    match r
        .output
        .graph_outputs
        .get(pin)
        .and_then(|b| b.0.first())
        .map(|i| &i.payload)
    {
        Some(Payload::PlantMesh(m)) => (**m).clone(),
        _ => panic!("no mesh on `{pin}`"),
    }
}

/// seed → grow (settings) → [extra chain] → output `out`.
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

fn starter(name: &str) -> ProcGraph {
    let text = regia_procgen::plant::starter(&format!("starter/{name}.pgraph.ron")).expect("starter ships");
    ProcGraph::from_ron(text).expect("starter parses")
}

#[test]
fn starters_compile_and_run() {
    for name in ["plant_broadleaf", "plant_conifer", "plant_bush"] {
        let r = run(&starter(name), 7);
        let m = mesh(&r, "mesh");
        assert!(m.triangles() > 0, "{name} meshes");
        assert!(
            m.slots.iter().any(|s| s.kind == SlotKind::Leaf),
            "{name} has leaves"
        );
        assert_eq!(r.exports().len(), 1, "{name} records one export");
        assert_eq!(r.exports()[0].settings.name, name);
    }
    let bush = skeleton(&run(&starter("plant_bush"), 7), "skeleton");
    assert_eq!(bush.plants.len(), 6, "a bush is a clump of six plants");
}

// 1. Growth hash.
#[test]
fn t01_broadleaf_seed_7_skeleton_hash_is_recorded() {
    let sk = skeleton(&run(&starter("plant_broadleaf"), 7), "skeleton");
    let n = sk.node_count();
    let h = hex(&sk.hash());
    // Re-recorded 2026-11-09 after `audit-g-procgen2-determinism` finding 1: `hash_into` now covers
    // yaw, light, dormancy, bud order and `shed_branches` (lead: bless on the build server reference).
    assert_eq!(
        (n, h.as_str()),
        (
            2600,
            "2c326504eea6ca38233da62a90e4a3dc7bc331b7f31d56c9d4f6317a57e92653"
        ),
        "broadleaf seed 7: node count and skeleton hash"
    );
    // Same inputs, same bytes.
    let again = skeleton(&run(&starter("plant_broadleaf"), 7), "skeleton");
    assert_eq!(sk.hash(), again.hash());
    let other = skeleton(&run(&starter("plant_broadleaf"), 8), "skeleton");
    assert_ne!(sk.hash(), other.hash(), "another seed grows another tree");
}

fn ratio(apical: f64) -> f64 {
    let a = format!("{apical:?}");
    let sk = skeleton(
        &run(
            &grow_graph(&[("apical", &a), ("growth_steps", "10"), ("vigor", "0.3")]),
            7,
        ),
        "out",
    );
    let p = &sk.plants[0];
    p.height() / p.crown_width().max(1e-6)
}

// 2. Apical control.
#[test]
fn t02_apical_control_raises_height_over_width() {
    let r: Vec<f64> = [0.4, 0.55, 0.7].into_iter().map(ratio).collect();
    assert!(
        r[0] < r[1] && r[1] < r[2],
        "height / crown width {r:?} increases with apical"
    );
}

fn side_counts(r: &RunResult) -> (usize, usize) {
    let sk = skeleton(r, "out");
    let p = &sk.plants[0];
    let root = p.nodes[0].position;
    let lateral = p.nodes.iter().filter(|n| n.order >= 1);
    let plus = lateral.clone().filter(|n| n.position[0] - root[0] > 0.05).count();
    let minus = lateral.filter(|n| n.position[0] - root[0] < -0.05).count();
    (plus, minus)
}

// 3. Light competition.
#[test]
fn t03_an_obstacle_on_one_side_halves_its_branches() {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "10")])),
            (3, node("graph.output", &[])),
            (4, node("graph.input", &[("pin", "\"obstacles\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (4, "out", 2, "obstacles"),
        ],
    );
    let free = side_counts(&run(&g, 7));
    let wall = Bundle::one(DataItem::new(Payload::Shape(box_shape(
        [3.0, 3.0, 0.0],
        [2.6, 4.0, 4.0],
        0.0,
    ))));
    let blocked = side_counts(&run_frame(
        &g,
        7,
        BTreeMap::from([("obstacles".to_string(), wall)]),
    ));
    assert!(free.0 > 10 && free.1 > 10, "free tree grows both ways: {free:?}");
    assert!(
        blocked.0 * 2 < blocked.1,
        "obstacle side {} vs free side {} (recorded)",
        blocked.0,
        blocked.1
    );
}

fn low_lateral(shed: &str) -> usize {
    let sk = skeleton(
        &run(
            &grow_graph(&[("growth_steps", "12"), ("shed_below_light", shed)]),
            7,
        ),
        "out",
    );
    let p = &sk.plants[0];
    let y0 = p.nodes[0].position[1];
    p.nodes
        .iter()
        .filter(|n| n.order >= 1 && n.position[1] - y0 < 2.0)
        .count()
}

// 4. Shedding.
#[test]
fn t04_shedding_removes_lower_interior_branches() {
    let base = low_lateral("0.12");
    let shed = low_lateral("0.5");
    assert!(base > 0, "default tree has low branches");
    assert!(shed * 2 < base, "order >= 1 nodes below 2 m: {base} -> {shed}");
}

// 5. Pipe model.
#[test]
fn t05_pipe_model_holds_at_every_fork() {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "8")])),
            (
                3,
                node("plant.radius", &[("base_swell", "[(0.0, 1.0), (1.0, 1.0)]")]),
            ),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let sk = skeleton(&run(&g, 7), "out");
    let p = &sk.plants[0];
    let e = 2.5f64;
    let children = p.children();
    let mut forks = 0;
    for (i, ch) in children.iter().enumerate() {
        if ch.len() < 2 {
            continue;
        }
        forks += 1;
        let rp = regia_procgen::detmath::pow(p.nodes[i].radius, e);
        let sum: f64 = ch
            .iter()
            .map(|&c| regia_procgen::detmath::pow(p.nodes[c as usize].radius, e))
            .sum();
        assert!((rp - sum).abs() <= 1e-6 * rp, "fork {i}: {rp} vs {sum}");
    }
    assert!(forks > 10, "the tree forks ({forks})");
}

// 6. Phyllotaxis.
#[test]
fn t06_successive_buds_diverge_by_the_golden_angle() {
    let sk = skeleton(
        &run(
            &grow_graph(&[
                ("growth_steps", "1"),
                ("wander_deg", "0.0"),
                ("light_pull", "0.0"),
                ("up_pull", "0.0"),
            ]),
            7,
        ),
        "out",
    );
    let p = &sk.plants[0];
    let trunk_buds: Vec<f64> = p
        .buds
        .iter()
        .filter(|b| p.nodes[b.node as usize].order == 0)
        .map(|b| regia_procgen::detmath::atan2(b.dir[0], b.dir[2]).to_degrees())
        .collect();
    assert!(trunk_buds.len() >= 3, "{} trunk buds", trunk_buds.len());
    for w in trunk_buds.windows(2) {
        let d = (w[1] - w[0]).rem_euclid(360.0);
        assert!((d - 137.5).abs() < 0.01, "divergence {d}");
    }
}

// 7. Mesh (integration; ring closure and the sides rule are unit tests in `nodes::plant_mesh`).
#[test]
fn t07_grown_tree_mesh_is_finite_with_unit_normals() {
    let m = mesh(&run(&starter("plant_broadleaf"), 7), "mesh");
    for s in &m.slots {
        assert_eq!(s.positions.len(), s.normals.len());
        assert_eq!(s.positions.len(), s.wind.len());
        assert_eq!(s.positions.len(), s.wind_pivot.len());
        assert!(s.positions.iter().flatten().all(|v| v.is_finite()), "no NaN");
        for n in &s.normals {
            let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((l - 1.0).abs() < 1e-4, "unit normals");
        }
        assert!(s.indices.iter().all(|&i| (i as usize) < s.positions.len()));
        let flutter = if s.kind == SlotKind::Leaf { 1.0 } else { 0.0 };
        assert!(
            s.wind.iter().all(|w| w[3] == flutter),
            "leaf flutter weight only on leaves"
        );
    }
}

fn leaves_graph(second: Option<&str>) -> ProcGraph {
    let bare = "[(card: true, weight: 1.0, size_m: [0.08, 0.1], rules: []), (leave_bare: true, weight: 0.0, rules: [height(0.0, 2.0, 10.0)])]";
    let mut nodes = vec![
        (1, node("plant.seed", &[])),
        (2, node("plant.grow", &[("growth_steps", "10")])),
        (3, node("plant.radius", &[])),
        (4, node("plant.mesh", &[])),
        (
            5,
            node(
                "plant.leaves",
                &[("choices", bare), ("orient_jitter_deg", "0.0"), ("orient", "sun")],
            ),
        ),
        (6, node("graph.output", &[])),
    ];
    let mut edges = vec![
        (1, "out", 2, "in"),
        (2, "out", 3, "in"),
        (3, "out", 4, "in"),
        (4, "out", 5, "in"),
    ];
    if let Some(clear) = second {
        nodes.push((7, node("plant.leaves", &[("keep_clear_m", clear)])));
        edges.push((5, "out", 7, "in"));
        edges.push((7, "out", 6, "in"));
    } else {
        edges.push((5, "out", 6, "in"));
    }
    plant_graph(nodes, &edges)
}

// 8. Leaves.
#[test]
fn t08_leave_bare_rule_and_keep_clear() {
    let r = run(&leaves_graph(None), 7);
    let m = mesh(&r, "out");
    assert!(!m.leaf_sites.is_empty(), "leaves were placed");
    let y0 = m.skeleton.plants[0].nodes[0].position[1];
    for c in &m.leaf_sites {
        assert!(
            f64::from(c[1]) - y0 >= 2.0 - 1e-4,
            "no leaf below 2 m (at {})",
            c[1]
        );
    }
    let first = m.leaf_sites.len();
    let two = mesh(&run(&leaves_graph(Some("0.3")), 7), "out");
    assert!(two.leaf_sites.len() > first, "the second pass placed leaves");
    let (a, b) = two.leaf_sites.split_at(first);
    for q in b {
        for p in a {
            let d = ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2) + (q[2] - p[2]).powi(2)).sqrt();
            assert!(d >= 0.3 - 1e-4, "second-pass leaf {d} m from a first-pass leaf");
        }
    }
}

// 10. Domain.
#[test]
fn t10_plant_nodes_are_plant_only_and_world_readers_world_only() {
    let mut g = grow_graph(&[]);
    g.flags.domain = Domain::World;
    let e = g
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("World graph rejects plant nodes");
    assert!(e.has("node_wrong_domain"));
    let g = plant_graph(
        vec![(1, node("in.self", &[])), (2, node("graph.output", &[]))],
        &[(1, "out", 2, "in")],
    );
    let e = g
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("Plant graph rejects world readers");
    assert!(e.has("node_wrong_domain"));
}

#[test]
fn export_needs_a_mesh_and_records_the_job() {
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "3")])),
            (3, node("plant.export", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let c = g.compile(&ProcNodeRegistry::with_builtins()).expect("compiles");
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 1);
    let err = Evaluator::new()
        .run(&c, &inputs, &CancelToken::new())
        .expect_err("a skeleton is not exportable");
    assert_eq!(err.code(), "plant_not_meshed");
    let r = run(&starter("plant_broadleaf"), 3);
    let job = &r.exports()[0];
    assert_eq!(job.out, "plant_broadleaf.glb");
    assert_eq!(job.settings.lods, vec![1.0, 0.5, 0.2, 0.05]);
    assert!(job.mesh.triangles() > 0);
}

#[test]
fn shaping_nodes_keep_the_tree_valid() {
    for (ty, settings) in [
        ("plant.bend", vec![]),
        ("plant.lean", vec![("deg", "15.0")]),
        ("plant.cut", vec![("by", "height"), ("max", "2.0")]),
        ("plant.simplify", vec![("angle_deg", "8.0")]),
        ("plant.scale", vec![("factor", "2.0")]),
        ("plant.twist", vec![("deg_per_node", "20.0")]),
    ] {
        let g = plant_graph(
            vec![
                (1, node("plant.seed", &[])),
                (2, node("plant.grow", &[("growth_steps", "6")])),
                (3, node("plant.radius", &[])),
                (4, node(ty, &settings)),
                (5, node("graph.output", &[])),
            ],
            &[
                (1, "out", 2, "in"),
                (2, "out", 3, "in"),
                (3, "out", 4, "in"),
                (4, "out", 5, "in"),
            ],
        );
        let before = skeleton(&run(&grow_graph(&[("growth_steps", "6")]), 7), "out");
        let sk = skeleton(&run(&g, 7), "out");
        let p = &sk.plants[0];
        assert!(!p.nodes.is_empty(), "{ty}");
        for (i, n) in p.nodes.iter().enumerate() {
            assert!(n.position.iter().all(|v| v.is_finite()), "{ty}: finite");
            if let Some(par) = n.parent {
                assert!((par as usize) < i, "{ty}: parents first");
            }
        }
        match ty {
            "plant.cut" => {
                let y0 = p.nodes[0].position[1];
                assert!(
                    p.nodes.iter().all(|n| n.position[1] - y0 <= 2.0 + 1e-9),
                    "cut at 2 m"
                );
                assert!(p.nodes.len() < before.plants[0].nodes.len());
            }
            "plant.simplify" => assert!(
                p.nodes.len() < before.plants[0].nodes.len(),
                "simplify removes nodes"
            ),
            "plant.scale" => {
                let h0 = before.plants[0].height();
                assert!((p.height() - 2.0 * h0).abs() < 1e-9 * h0.max(1.0));
            }
            _ => {}
        }
    }
}

#[test]
fn attach_and_envelope_growth() {
    // Envelope growth: every new node stays near the envelope sphere.
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow_envelope", &[("points", "400")])),
            (3, node("graph.output", &[])),
            (4, node("graph.input", &[("pin", "\"env\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (4, "out", 2, "envelope"),
        ],
    );
    let sphere = regia_procgen::data::ShapeData {
        form: regia_procgen::data::ShapeForm::Sphere { radius: 1.5 },
        position: [0.0, 2.5, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        edge_m: 0.0,
        clip_xz: None,
    };
    let env = Bundle::one(DataItem::new(Payload::Shape(sphere)));
    let sk = skeleton(
        &run_frame(&g, 5, BTreeMap::from([("env".to_string(), env)])),
        "out",
    );
    let p = &sk.plants[0];
    assert!(p.nodes.len() > 20, "colonization grew {} nodes", p.nodes.len());
    let top = p.nodes.iter().map(|n| n.position[1]).fold(0.0, f64::max);
    assert!(top > 1.5 && top < 4.3, "crown inside the envelope (top {top})");

    // Attach: twigs on every tip of a small tree.
    let g = plant_graph(
        vec![
            (1, node("plant.seed", &[])),
            (2, node("plant.grow", &[("growth_steps", "4")])),
            (3, node("plant.seed", &[])),
            (
                4,
                node("plant.grow", &[("growth_steps", "2"), ("internode_m", "0.1")]),
            ),
            (5, node("plant.attach", &[("scale_by_radius", "false")])),
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
    let base = skeleton(&run(&grow_graph(&[("growth_steps", "4")]), 7), "out");
    let sk = skeleton(&run(&g, 7), "out");
    assert!(
        sk.plants[0].nodes.len() > base.plants[0].nodes.len(),
        "parts were attached"
    );
    for (i, n) in sk.plants[0].nodes.iter().enumerate() {
        if let Some(par) = n.parent {
            assert!((par as usize) < i);
        }
    }
    assert!(sk.plants[0].nodes.iter().all(|n| n.bud != BudState::Bloom));
}

// 12. Warm-cache determinism: a cached `plant.grow` must not serve a skeleton another run would
// not produce (`audit-g-procgen2-determinism` finding 1).
#[test]
fn t12_grow_cache_misses_when_only_the_seed_facing_changes() {
    use parking_lot::Mutex;
    use regia_procgen::exec::ProcCache;

    fn facing_graph(facing: &str) -> ProcGraph {
        plant_graph(
            vec![
                (1, node("plant.seed", &[("facing_deg", facing)])),
                (2, node("plant.grow", &[])),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        )
    }
    let cache = Arc::new(Mutex::new(ProcCache::new(64 << 20)));
    let ev = Evaluator::with_cache(cache);
    let run = |facing: &str| {
        let g = facing_graph(facing);
        let c = match g.compile(&ProcNodeRegistry::with_builtins()) {
            Ok(c) => c,
            Err(e) => panic!("compile failed: {:?}", e.0),
        };
        let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 7);
        match ev.run(&c, &inputs, &CancelToken::new()) {
            Ok(r) => skeleton(&r, "out"),
            Err(e) => panic!("run failed: {e}"),
        }
    };
    let first = run("0.0");
    let second = run("90.0");
    assert!(
        !first.plants[0].buds.is_empty(),
        "the default grow run places lateral buds"
    );
    assert_ne!(
        first.hash(),
        second.hash(),
        "facing_deg lives in the root yaw, so the skeleton hash must change"
    );
    assert!(
        first.plants[0]
            .buds
            .iter()
            .zip(&second.plants[0].buds)
            .any(|(a, b)| a.dir != b.dir),
        "a warm cache must not serve the yaw-0 plant for a 90-degree facing"
    );
}
