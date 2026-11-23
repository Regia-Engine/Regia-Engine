// SPDX-License-Identifier: MPL-2.0
//! Mesh envelopes (spec 68 § 4, ADR-2731): buds and attraction points stay inside a closed mesh, and a
//! mesh with no inside fails the run with its documented code.

mod common;

use std::sync::Arc;

use common::{compile, node};
use regia_procgen::data::{MeshData, Payload};
use regia_procgen::exec::StaticSnapshot;
use regia_procgen::nodes::Domain;
use regia_procgen::plant::envelope::{MeshEnvelope, torus_mesh};
use regia_procgen::{CancelToken, Edge, Evaluator, NodeEntry, ProcGraph, RunError, RunInputs, RunResult};

const MAJOR: f64 = 2.0;
const MINOR: f64 = 0.75;

fn plant_graph(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> ProcGraph {
    let mut g = common::graph(nodes, edges);
    g.flags.domain = Domain::Plant;
    g
}

fn snapshot(mesh: MeshData) -> Arc<StaticSnapshot> {
    let mut snap = StaticSnapshot::default();
    snap.meshes.insert("env.glb".to_string(), Arc::new(mesh));
    Arc::new(snap)
}

/// A seed at the tube's center on +X, grown toward the mesh envelope.
fn seed_nodes() -> Vec<(u32, NodeEntry)> {
    vec![
        (
            1,
            node("gen.points", &[("points", "[(position: (2.0, 0.0, 0.0))]")]),
        ),
        (2, node("plant.seed", &[])),
        (3, node("plant.envelope_mesh", &[("asset", "\"env.glb\"")])),
    ]
}

fn grow_graph(steps: u32) -> ProcGraph {
    let mut nodes = seed_nodes();
    nodes.push((
        4,
        node(
            "plant.grow",
            &[
                ("growth_steps", &steps.to_string()),
                ("internode_m", "0.2"),
                ("wander_deg", "30.0"),
                ("shed_below_light", "0.0"),
            ],
        ),
    ));
    plant_graph(
        nodes,
        &[
            (1, "out", 2, "in"),
            (2, "out", 4, "in"),
            (3, "out", 4, "envelope"),
        ],
    )
}

fn run(g: &ProcGraph, mesh: MeshData, seed: u32) -> Result<RunResult, RunError> {
    let mut inputs = RunInputs::new(snapshot(mesh), seed);
    inputs.keep_debug = true;
    Evaluator::new().run(&compile(g), &inputs, &CancelToken::new())
}

/// Every skeleton node of node `id`'s output.
fn positions(r: &RunResult, id: u32) -> Vec<[f64; 3]> {
    let debug = r.debug.as_ref().unwrap_or_else(|| panic!("debug outputs"));
    let mut out = Vec::new();
    for item in &debug[&id][0].0 {
        if let Payload::PlantSkeleton(sk) = &item.payload {
            for p in &sk.plants {
                out.extend(p.nodes.iter().map(|n| n.position));
            }
        }
    }
    out
}

/// Inside the true torus the mesh is inscribed in: the faceted mesh lies within it, so a point in the
/// mesh must satisfy this. An independent check of the envelope rule.
fn inside_true_torus(p: [f64; 3]) -> bool {
    let ring = (p[0] * p[0] + p[2] * p[2]).sqrt() - MAJOR;
    ring * ring + p[1] * p[1] <= MINOR * MINOR * 1.0001
}

/// 8 seeds of 63 growth steps each: 504 seeded steps (one run allows 64).
#[test]
fn buds_stay_inside_a_torus_for_500_seeded_steps() {
    let torus = torus_mesh(MAJOR as f32, MINOR as f32, 48, 24);
    let envelope = MeshEnvelope::new(&torus).unwrap_or_else(|e| panic!("{e:?}"));
    let g = grow_graph(63);
    let mut total = 0usize;
    let mut reach = 0.0f64;
    for seed in 0..8 {
        let r = run(&g, torus.clone(), seed).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let nodes = positions(&r, 4);
        assert!(nodes.len() > 1, "seed {seed}: the plant grew");
        for p in &nodes {
            assert!(envelope.contains(*p), "seed {seed}: {p:?} left the mesh");
            assert!(inside_true_torus(*p), "seed {seed}: {p:?} left the torus");
            reach = reach.max((p[0] - 2.0).abs().max(p[2].abs()));
        }
        total += nodes.len();
    }
    assert!(total > 100, "{total} nodes grew");
    // Growth pressed on the envelope instead of staying near the seed.
    assert!(reach > 0.3, "reach {reach}");
}

#[test]
fn a_larger_envelope_lets_the_same_plant_grow_more() {
    let g = grow_graph(16);
    let small = positions(
        &run(&g, torus_mesh(2.0, 0.3, 48, 24), 3).unwrap_or_else(|e| panic!("{e}")),
        4,
    );
    let large = positions(
        &run(&g, torus_mesh(2.0, 1.2, 48, 24), 3).unwrap_or_else(|e| panic!("{e}")),
        4,
    );
    assert!(large.len() > small.len(), "{} vs {}", large.len(), small.len());
}

#[test]
fn space_colonization_samples_attraction_points_inside_the_mesh() {
    let torus = torus_mesh(MAJOR as f32, MINOR as f32, 48, 24);
    let mut nodes = seed_nodes();
    nodes.push((
        4,
        node(
            "plant.grow_envelope",
            &[
                ("growth_steps", "80"),
                ("points", "400"),
                ("step_m", "0.15"),
                ("influence_m", "1.0"),
            ],
        ),
    ));
    let g = plant_graph(
        nodes,
        &[
            (1, "out", 2, "in"),
            (2, "out", 4, "in"),
            (3, "out", 4, "envelope"),
        ],
    );
    let r = run(&g, torus.clone(), 5).unwrap_or_else(|e| panic!("{e}"));
    let grown = positions(&r, 4);
    assert!(grown.len() > 20, "{} nodes", grown.len());
    // Nodes step toward points in the mesh; a step of `step_m` may overshoot a point by that much.
    for p in &grown {
        let ring = (p[0] * p[0] + p[2] * p[2]).sqrt() - MAJOR;
        assert!(
            (ring * ring + p[1] * p[1]).sqrt() <= MINOR + 0.15,
            "{p:?} is off the torus"
        );
    }
    // Around the ring, not only near the seed: the torus's hole was never crossed.
    assert!(
        grown
            .iter()
            .all(|p| (p[0] * p[0] + p[2] * p[2]).sqrt() > MAJOR - MINOR - 0.15)
    );
    let again = run(&g, torus, 5).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(r.output_hash, again.output_hash);
    assert_eq!(positions(&again, 4), grown, "the same seed grows the same plant");
}

#[test]
fn a_bad_envelope_fails_the_run_with_its_code() {
    let g = grow_graph(4);
    let code = |mesh: MeshData| match run(&g, mesh, 1) {
        Err(RunError::Node { code, .. }) => code,
        other => panic!("expected a node error: {other:?}"),
    };
    let empty = MeshData {
        asset: "env.glb".to_string(),
        positions: Vec::new(),
        normals: Vec::new(),
        indices: Vec::new(),
    };
    assert_eq!(code(empty), "envelope_empty");
    let mut open = torus_mesh(2.0, 0.75, 12, 6);
    open.indices.truncate(open.indices.len() - 6);
    assert_eq!(code(open), "envelope_not_manifold");
    let mut three = torus_mesh(2.0, 0.75, 12, 6);
    let again: Vec<u32> = three.indices[..3].to_vec();
    three.indices.extend(again);
    assert_eq!(code(three), "envelope_not_manifold");
    // The host has no such mesh.
    let missing = Evaluator::new().run(
        &compile(&g),
        &RunInputs::new(Arc::new(StaticSnapshot::default()), 1),
        &CancelToken::new(),
    );
    assert!(
        matches!(&missing, Err(RunError::Node { code, .. }) if code == "envelope_empty"),
        "{missing:?}"
    );
}

/// A `Shape` envelope still takes the region rule; the pin holds both kinds.
#[test]
fn a_shape_and_a_mesh_on_one_pin_are_a_union() {
    let torus = torus_mesh(2.0, 0.4, 24, 12);
    let mut nodes = seed_nodes();
    nodes.push((5, node("graph.input", &[("pin", "\"shape\"")])));
    nodes.push((
        4,
        node("plant.grow", &[("growth_steps", "12"), ("internode_m", "0.2")]),
    ));
    let g = plant_graph(
        nodes,
        &[
            (1, "out", 2, "in"),
            (2, "out", 4, "in"),
            (3, "out", 4, "envelope"),
            (5, "out", 4, "envelope"),
        ],
    );
    let sphere = regia_procgen::data::ShapeData {
        form: regia_procgen::data::ShapeForm::Sphere { radius: 1.0 },
        position: [2.0, 1.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        edge_m: 0.0,
        clip_xz: None,
    };
    let frame = regia_procgen::exec::Frame {
        graph_inputs: [(
            "shape".to_string(),
            regia_procgen::Bundle::one(regia_procgen::DataItem::new(Payload::Shape(sphere))),
        )]
        .into(),
        ..regia_procgen::exec::Frame::default()
    };
    let mut inputs = RunInputs::new(snapshot(torus.clone()), 2);
    inputs.keep_debug = true;
    let r = Evaluator::new()
        .run_frame(&compile(&g), &inputs, &CancelToken::new(), &frame)
        .unwrap_or_else(|e| panic!("{e}"));
    let grown = positions(&r, 4);
    assert!(
        grown.iter().any(|p| p[1] > 0.5),
        "grew up out of the tube into the sphere"
    );
    let envelope = MeshEnvelope::new(&torus).unwrap_or_else(|e| panic!("{e:?}"));
    for p in &grown {
        let in_sphere = (p[0] - 2.0).powi(2) + (p[1] - 1.0).powi(2) + p[2] * p[2] <= 1.0001;
        assert!(in_sphere || envelope.contains(*p), "{p:?} is in neither");
    }
}

#[test]
fn a_kind_the_envelope_pin_does_not_take_is_refused() {
    let mut g = grow_graph(1);
    g.nodes.insert(9, node("gen.one", &[]));
    g.edges.push(Edge::new(9, "out", 4, "envelope"));
    assert!(
        g.compile(&regia_procgen::ProcNodeRegistry::with_builtins())
            .is_err()
    );
}
