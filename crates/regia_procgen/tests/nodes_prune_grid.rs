// SPDX-License-Identifier: MPL-2.0
//! What `points.prune`'s broad-phase grid must keep whatever its layout (ADR-2724 (2)): the rows kept
//! equal an independent all-pairs pass over the same visit order, and a query meets the boxes of a
//! cell in the order they were kept, which decides the `seam_mismatch_risk` count of tiled runs.

mod common;

use std::sync::Arc;

use common::{compile, graph, node};
use regia_procgen::data::{DataItem, Payload, Point, PointSet};
use regia_procgen::exec::{Frame, RunResult, StaticSnapshot};
use regia_procgen::{Bundle, CancelToken, Evaluator, RunInputs};

const PRUNE_NODE: u32 = 2;

fn boxed(position: [f32; 3], half: [f32; 3], seed: u32) -> Point {
    Point {
        position,
        bounds_min: half.map(|v| -v),
        bounds_max: half,
        seed,
        ..Point::default()
    }
}

fn run(p: PointSet, settings: &[(&str, &str)], run_rect: Option<[f32; 4]>) -> RunResult {
    let compiled = compile(&graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (PRUNE_NODE, node("points.prune", settings)),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", PRUNE_NODE, "in"), (PRUNE_NODE, "out", 3, "in")],
    ));
    let frame = Frame {
        graph_inputs: [("in".to_string(), Bundle::one(DataItem::points(p)))]
            .into_iter()
            .collect(),
        ..Frame::default()
    };
    let mut inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 5);
    inputs.run_rect = run_rect;
    Evaluator::new()
        .run_frame(&compiled, &inputs, &CancelToken::new(), &frame)
        .unwrap_or_else(|e| panic!("prune run: {e}"))
}

fn kept_positions(r: &RunResult) -> Vec<[f32; 3]> {
    let out = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
    match out.0.first().map(|item| &item.payload) {
        Some(Payload::Points(p)) => p.position.to_vec(),
        other => panic!("no points out: {other:?}"),
    }
}

/// Mixed sizes in a few height layers: small boxes in one cell, large ones across many, overlaps in
/// XZ that the height separates, and repeated positions. A plain linear generator, so the set does
/// not depend on anything under test.
fn mixed_set(n: usize) -> PointSet {
    let mut state = 12_345u32;
    let mut next = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut p = PointSet::new();
    for i in 0..n {
        let snap = i % 11 == 0;
        let (x, z) = (next() * 300.0, next() * 300.0);
        let (x, z) = if snap { (x.floor(), z.floor()) } else { (x, z) };
        let layer = (next() * 4.0).floor() * 7.0;
        let big = next() < 0.02;
        let half = if big {
            [10.0 + next() * 20.0, 1.0, 10.0 + next() * 20.0]
        } else {
            [0.1 + next() * 2.0, 0.5 + next(), 0.1 + next() * 2.0]
        };
        p.push(boxed([x, layer, z], half, (i as u32).wrapping_mul(2_654_435_761)));
    }
    p
}

/// The node's contract written out: visit in `order`, keep a row whose padded box overlaps no kept
/// box. Returns the kept rows in input order.
fn all_pairs(p: &PointSet, order: &[usize], pad: f32) -> Vec<[f32; 3]> {
    let boxes: Vec<([f32; 3], [f32; 3])> = (0..p.len())
        .map(|i| {
            let (lo, hi) = p.world_aabb(i);
            (lo.map(|v| v - pad), hi.map(|v| v + pad))
        })
        .collect();
    let overlap =
        |a: usize, b: usize| (0..3).all(|k| boxes[a].0[k] < boxes[b].1[k] && boxes[b].0[k] < boxes[a].1[k]);
    let mut kept: Vec<usize> = Vec::new();
    for &i in order {
        if !kept.iter().any(|&j| overlap(i, j)) {
            kept.push(i);
        }
    }
    kept.sort_unstable();
    kept.into_iter().map(|i| p.position[i]).collect()
}

#[test]
fn the_kept_rows_equal_an_all_pairs_pass_in_the_same_order() {
    let p = mixed_set(3000);
    for (pad, pad_text) in [(0.0f32, "0.0"), (0.25, "0.25")] {
        let as_is: Vec<usize> = (0..p.len()).collect();
        let got = run(
            p.clone(),
            &[("order", "\"as_is\""), ("padding_m", pad_text)],
            None,
        );
        assert_eq!(
            kept_positions(&got),
            all_pairs(&p, &as_is, pad),
            "as_is, pad {pad}"
        );

        // `large_first` without a tolerance: box volume down, then seed, then row.
        let volume = |i: usize| {
            let (lo, hi) = p.world_aabb(i);
            let (lo, hi) = (lo.map(|v| v - pad), hi.map(|v| v + pad));
            f64::from((hi[0] - lo[0]) * (hi[1] - lo[1]) * (hi[2] - lo[2]))
        };
        let mut large: Vec<usize> = (0..p.len()).collect();
        large.sort_by(|&a, &b| {
            volume(b)
                .total_cmp(&volume(a))
                .then(p.seed[a].cmp(&p.seed[b]))
                .then(a.cmp(&b))
        });
        let got = run(
            p.clone(),
            &[("order", "\"large_first\""), ("padding_m", pad_text)],
            None,
        );
        assert_eq!(
            kept_positions(&got),
            all_pairs(&p, &large, pad),
            "large_first, pad {pad}"
        );
    }
}

#[test]
fn a_query_meets_the_boxes_of_a_cell_in_the_order_they_were_kept() {
    // `a` and `b` do not overlap and share grid cell 0 (cells are 20 m); `c` overlaps both and comes
    // last. Only `a` lies in the seam band of the run rectangle, so the count says which box `c` met
    // first: the one kept first.
    let a = boxed([-6.0, 0.0, 0.0], [5.0; 3], 1);
    let b = boxed([6.0, 0.0, 0.0], [5.0; 3], 2);
    let c = boxed([0.0, 0.0, 0.0], [5.0; 3], 3);
    let rect = Some([-6.5, -1000.0, 1000.0, 1000.0]);
    let seam = |rows: &[&Point]| {
        let mut p = PointSet::new();
        for row in rows {
            p.push(**row);
        }
        let r = run(p, &[("order", "\"as_is\"")], rect);
        assert_eq!(kept_positions(&r).len(), 2, "`c` is dropped");
        let line = r
            .report
            .node(PRUNE_NODE)
            .unwrap_or_else(|| panic!("no report line"));
        line.counters.get("seam_mismatch_risk").copied().unwrap_or(0)
    };
    assert_eq!(
        seam(&[&a, &b, &c]),
        1,
        "`a` was kept first, so `c` meets it first"
    );
    assert_eq!(
        seam(&[&b, &a, &c]),
        0,
        "`b` was kept first, so `c` meets it first"
    );
}
