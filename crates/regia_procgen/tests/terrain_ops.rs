// SPDX-License-Identifier: MPL-2.0
//! The heightfield-operator nodes (DO27 absorb row C-18, ADR-8465): `terrain.flatten_pad`,
//! `terrain.crater`, `terrain.terrace`, `terrain.domain_warp` and `terrain.noise` as bake-only
//! terrain edits. No GPU.

mod common;

use std::sync::Arc;

use common::{box_shape, compile, graph, node};
use regia_procgen::data::{GroundData, Point, PointSet};
use regia_procgen::exec::{Frame, SelfInfo, StaticSnapshot, TerrainEditGate, TerrainEditRecord};
use regia_procgen::{Bundle, CancelToken, DataItem, Evaluator, Payload, ProcGraph, RunInputs, RunResult};
use regia_terrain::TerrainMap;
use regia_terrain::ops::{GridFrame, NoiseParams, fbm};

/// A 512 m map, 2 m per sample, rough to about ±15 m.
fn rough_map() -> TerrainMap {
    let n = 257u32;
    let flat = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 512.0, 1.0, 0.0).expect("map");
    let mut p = NoiseParams::new(11, 15.0, 120.0, 5);
    p.grain_angle_deg = 30.0;
    let heights = fbm(&GridFrame::of_map(&flat), &p).expect("noise");
    TerrainMap::from_heights(n, n, heights, 512.0, 1.0, 0.0).expect("map")
}

const RECT: &str = "[-200.0, -200.0, 200.0, 200.0]";

fn snapshot_of(map: &TerrainMap, rect: [f32; 4]) -> StaticSnapshot {
    StaticSnapshot {
        self_info: Some(SelfInfo {
            guid: regia_core::id::Guid(7),
            name: "gen".into(),
            bounds: box_shape([0.0; 3], [300.0; 3], 0.0),
        }),
        ground: Some(Arc::new(GroundData::capture(map, rect, 2))),
        ..StaticSnapshot::default()
    }
}

fn points(at: &[[f32; 3]]) -> Bundle {
    let mut set = PointSet::new();
    for p in at {
        set.push(Point {
            position: *p,
            ..Point::default()
        });
    }
    Bundle::one(DataItem::new(Payload::Points(set)))
}

fn run(
    g: &ProcGraph,
    snap: StaticSnapshot,
    pads: Option<Bundle>,
    gate: TerrainEditGate,
) -> Result<RunResult, regia_procgen::RunError> {
    let frame = Frame {
        graph_inputs: pads.map(|b| ("pts".to_string(), b)).into_iter().collect(),
        ..Frame::default()
    };
    let mut ri = RunInputs::new(Arc::new(snap), 5);
    ri.terrain_edits = gate;
    Evaluator::new().run_frame(&compile(g), &ri, &CancelToken::new(), &frame)
}

/// Settings written `name=RON;name=RON`.
fn settings(text: &str) -> Vec<(&str, &str)> {
    text.split(';')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

/// `graph.input("pts")` and `in.ground` into a point-driven node, out to `graph.output`.
fn point_graph(ty: &str, set: &str) -> ProcGraph {
    graph(
        vec![
            (1, node("graph.input", &[("pin", "\"pts\"")])),
            (2, node("in.ground", &[("rect", RECT)])),
            (3, node(ty, &settings(set))),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "ground"), (3, "out", 4, "in")],
    )
}

/// `in.ground` through a chain of ground-only nodes to `graph.output`.
fn ground_chain(nodes: &[(&str, &str)]) -> ProcGraph {
    let mut list = vec![(1, node("in.ground", &[("rect", RECT)]))];
    let mut edges = Vec::new();
    for (k, (ty, s)) in nodes.iter().enumerate() {
        let id = k as u32 + 2;
        list.push((id, node(ty, &settings(s))));
        edges.push((id - 1, "out", id, "ground"));
    }
    let last = nodes.len() as u32 + 1;
    list.push((last + 1, node("graph.output", &[])));
    edges.push((last, "out", last + 1, "in"));
    graph(list, &edges)
}

/// The map with `records` written in order.
fn applied(map: &TerrainMap, records: &[TerrainEditRecord]) -> Vec<f32> {
    let w = map.width() as usize;
    let mut h = map.heights().to_vec();
    for rec in records {
        let rw = (rec.rect[2] - rec.rect[0]) as usize;
        for (k, v) in rec.samples.iter().enumerate() {
            let (c, r) = (rec.rect[0] as usize + k % rw, rec.rect[1] as usize + k / rw);
            h[r * w + c] = *v;
        }
    }
    h
}

/// The graph output's ground heights.
fn out_ground(r: &RunResult) -> Arc<GroundData> {
    r.output
        .graph_outputs
        .get("out")
        .and_then(|b| b.0.first())
        .and_then(|i| match &i.payload {
            Payload::Ground(g) => Some(g.clone()),
            _ => None,
        })
        .expect("a ground output")
}

/// Row C-18's acceptance through the node: pads baked as terrain edits, written into the terrain,
/// read back flat within 1 cm wherever the bilinear surface lies wholly inside the footprint.
#[test]
fn flatten_pad_node_is_flat_within_1cm() {
    let map = rough_map();
    let pads = [[30.0, 4.5, -40.0], [-90.0, -2.0, 75.0]];
    for (height, radius) in [("\"mean\"", 25.0), ("\"point\"", 18.7)] {
        let g = point_graph(
            "terrain.flatten_pad",
            &format!("radius_m={radius:.1};height={height};feather_m=15.0"),
        );
        let r = run(
            &g,
            snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]),
            Some(points(&pads)),
            TerrainEditGate::Allowed,
        )
        .expect("bake");
        assert_eq!(r.output.terrain_edits.len(), 1, "one record for the node");
        let heights = applied(&map, &r.output.terrain_edits);
        let after = TerrainMap::from_heights(257, 257, heights, 512.0, 1.0, 0.0).expect("map");
        for p in pads {
            let mut seen = Vec::new();
            for i in -40..=40 {
                for j in -40..=40 {
                    let (u, v) = (f64::from(i) * 0.5, f64::from(j) * 0.5);
                    if (u * u + v * v).sqrt() > radius - 2.0 * 2f64.sqrt() {
                        continue;
                    }
                    let (x, z) = (f64::from(p[0]) + u, f64::from(p[2]) + v);
                    seen.push(after.sample_bilinear(x as f32, z as f32));
                }
            }
            let lo = seen.iter().copied().fold(f32::INFINITY, f32::min);
            let hi = seen.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            assert!(hi - lo <= 0.01, "{height} pad at {p:?}: {lo} .. {hi}");
            if height == "\"point\"" {
                assert!((lo - p[1]).abs() <= 0.01, "pad at the point's y {}: {lo}", p[1]);
            }
        }
    }
}

#[test]
fn every_terrain_op_node_respects_the_edit_gates() {
    let map = rough_map();
    let ground_nodes = ["terrain.terrace", "terrain.domain_warp", "terrain.noise"];
    for (gate, code) in [
        (TerrainEditGate::NotAllowed, "terrain_edits_not_allowed"),
        (TerrainEditGate::NotBaked, "flatten_runtime_forbidden"),
    ] {
        for ty in ground_nodes {
            let err = run(
                &ground_chain(&[(ty, "")]),
                snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]),
                None,
                gate,
            )
            .expect_err("gated");
            assert_eq!(err.code(), code, "{ty}");
        }
        for ty in ["terrain.crater", "terrain.flatten_pad"] {
            let err = run(
                &point_graph(ty, ""),
                snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]),
                Some(points(&[[0.0; 3]])),
                gate,
            )
            .expect_err("gated");
            assert_eq!(err.code(), code, "{ty}");
        }
    }
}

/// Warp → terrace → noise: one record per node in node order; applied in order they give the
/// output ground, unwound in reverse they give the terrain back bit for bit; a second bake is the
/// same bake.
#[test]
fn chained_ops_apply_in_order_and_unwind_in_reverse() {
    let map = rough_map();
    let g = ground_chain(&[
        ("terrain.domain_warp", "strength_m=12.0"),
        ("terrain.terrace", "step_height_m=3.0;edge_sharpness=1.0"),
        ("terrain.noise", "amplitude_m=0.5;wavelength_m=40.0"),
    ]);
    let snap = || snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]);
    let r = run(&g, snap(), None, TerrainEditGate::Allowed).expect("bake");
    let recs = &r.output.terrain_edits;
    assert_eq!(recs.len(), 3);
    // In order: the terrain inside the capture equals the output ground.
    let heights = applied(&map, recs);
    let out = out_ground(&r);
    let w = map.width() as usize;
    for (k, v) in out.heights.iter().enumerate() {
        let (c, row) = (
            out.origin[0] as usize + k % out.size[0] as usize,
            out.origin[1] as usize + k / out.size[0] as usize,
        );
        assert_eq!(heights[row * w + c].to_bits(), v.to_bits());
    }
    // Each record's originals are the previous node's output.
    let mut now = map.heights().to_vec();
    for rec in recs {
        let rw = (rec.rect[2] - rec.rect[0]) as usize;
        for (k, o) in rec.original.iter().enumerate() {
            let (c, row) = (rec.rect[0] as usize + k % rw, rec.rect[1] as usize + k / rw);
            assert_eq!(now[row * w + c].to_bits(), o.to_bits());
            now[row * w + c] = rec.samples[k];
        }
    }
    // Reverse: originals back where the record still holds its value.
    for rec in recs.iter().rev() {
        let rw = (rec.rect[2] - rec.rect[0]) as usize;
        for (k, o) in rec.original.iter().enumerate() {
            let (c, row) = (rec.rect[0] as usize + k % rw, rec.rect[1] as usize + k / rw);
            if now[row * w + c].to_bits() == rec.samples[k].to_bits() {
                now[row * w + c] = *o;
            }
        }
    }
    assert!(
        now.iter()
            .zip(map.heights())
            .all(|(a, b)| a.to_bits() == b.to_bits())
    );
    let again = run(&g, snap(), None, TerrainEditGate::Allowed).expect("again");
    assert_eq!(again.output_hash, r.output_hash);
    assert_eq!(&again.output.terrain_edits, recs);
}

/// Noise is a function of the full-terrain sample index: two different captures of one map give
/// a shared sample the same bits.
#[test]
fn noise_and_terrace_agree_across_captures() {
    let map = rough_map();
    let g = ground_chain(&[
        (
            "terrain.noise",
            "kind=\"ridged\";amplitude_m=6.0;grain_angle_deg=35.0;anisotropy=3.0",
        ),
        ("terrain.terrace", "width_jitter=0.5;collapse=0.4"),
    ]);
    let a = out_ground(
        &run(
            &g,
            snapshot_of(&map, [-200.0, -200.0, 20.0, 20.0]),
            None,
            TerrainEditGate::Allowed,
        )
        .expect("a"),
    );
    let b = out_ground(
        &run(
            &g,
            snapshot_of(&map, [-60.0, -60.0, 160.0, 160.0]),
            None,
            TerrainEditGate::Allowed,
        )
        .expect("b"),
    );
    let mut shared = 0;
    for (k, v) in a.heights.iter().enumerate() {
        let (c, r) = (
            a.origin[0] + k as u32 % a.size[0],
            a.origin[1] + k as u32 / a.size[0],
        );
        if c >= b.origin[0] && r >= b.origin[1] && c < b.origin[0] + b.size[0] && r < b.origin[1] + b.size[1]
        {
            let j = ((r - b.origin[1]) * b.size[0] + (c - b.origin[0])) as usize;
            assert_eq!(v.to_bits(), b.heights[j].to_bits(), "sample ({c}, {r})");
            shared += 1;
        }
    }
    assert!(shared > 1000, "{shared} shared samples");
}

#[test]
fn crater_node_stamps_each_point_and_a_lost_pad_warns() {
    let map = rough_map();
    let g = point_graph("terrain.crater", "radius_m=20.0;depth_m=6.0;rim_height_m=1.5");
    let r = run(
        &g,
        snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]),
        Some(points(&[[0.0, 0.0, 0.0], [100.0, 0.0, 100.0]])),
        TerrainEditGate::Allowed,
    )
    .expect("bake");
    let out = out_ground(&r);
    let at = |x: f32, z: f32| {
        let [c, row] = map.world_to_grid(x, z);
        let (c, row) = (
            c.round() as u32 - out.origin[0],
            row.round() as u32 - out.origin[1],
        );
        out.heights[(row * out.size[0] + c) as usize]
    };
    for (x, z) in [(0.0, 0.0), (100.0, 100.0)] {
        let before = map.sample_bilinear(x, z);
        assert!(
            (at(x, z) - (before - 6.0)).abs() < 1e-3,
            "crater floor at ({x}, {z})"
        );
    }
    // A pad whose footprint misses the capture is skipped with `pad_empty`.
    let pad = point_graph("terrain.flatten_pad", "radius_m=5.0");
    let r = run(
        &pad,
        snapshot_of(&map, [-200.0, -200.0, 200.0, 200.0]),
        Some(points(&[[240.0, 0.0, 240.0]])),
        TerrainEditGate::Allowed,
    )
    .expect("bake");
    assert!(r.report.warning_codes().iter().any(|c| c == "pad_empty"));
    assert!(r.output.terrain_edits.is_empty());
}
