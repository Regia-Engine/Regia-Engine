// SPDX-License-Identifier: MPL-2.0
//! `points.jitter` rewrites its position, rotation and scale columns in place on an input it owns
//! (ADR-2724 (2)). These pin what that must keep: the serial and pooled schedules agree, the other
//! columns pass through untouched, a set whose read columns have unequal lengths keeps the rows past
//! the shortest one exactly as they were, and a non-finite result is still caught.

mod common;

use std::sync::Arc;

use common::{compile, graph, node};
use regia_procgen::data::{DataItem, Payload, Point, PointSet};
use regia_procgen::exec::{Frame, Parallelism, StaticSnapshot};
use regia_procgen::{Bundle, CancelToken, CompiledGraph, Evaluator, RunInputs};

/// More than two 4096-row chunks, so the pooled run really splits.
const N: usize = 10_000;

fn points() -> PointSet {
    let mut p = PointSet::new();
    for i in 0..N {
        let f = i as f32;
        p.push(Point {
            position: [f * 0.25, (f * 0.01).sin(), -f * 0.5],
            rotation: [0.0, (f * 0.001).sin(), 0.0, (f * 0.001).cos()],
            scale: [1.0 + f * 1.0e-4, 1.0, 0.5],
            color: [f / N as f32, 0.5, 0.25, 1.0],
            density: (i % 7) as f32 / 7.0,
            seed: (i as u32).wrapping_mul(2_654_435_761),
            hardness: 0.5,
            ..Point::default()
        });
    }
    p
}

fn jitter() -> CompiledGraph {
    compile(&graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node(
                    "points.jitter",
                    &[
                        ("offset", "[(-0.2, 0.0, -0.2), (0.2, 0.5, 0.2)]"),
                        ("rotate_deg", "[(0.0, 0.0, 0.0), (10.0, 360.0, 0.0)]"),
                        ("scale", "[(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)]"),
                        ("scale_mode", "\"mul\""),
                        ("rotate_mode", "\"add\""),
                    ],
                ),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    ))
}

fn run(parallelism: Parallelism, p: PointSet) -> PointSet {
    let frame = Frame {
        graph_inputs: [("in".to_string(), Bundle::one(DataItem::points(p)))]
            .into_iter()
            .collect(),
        ..Frame::default()
    };
    let r = Evaluator::new()
        .with_parallelism(parallelism)
        .run_frame(
            &jitter(),
            &RunInputs::new(Arc::new(StaticSnapshot::default()), 11),
            &CancelToken::new(),
            &frame,
        )
        .unwrap_or_else(|e| panic!("jitter run: {e}"));
    let out = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
    match out.0.into_iter().next().map(|i| i.payload) {
        Some(Payload::Points(p)) => p,
        _ => panic!("jitter gives one points item"),
    }
}

fn bits3(v: &[[f32; 3]]) -> Vec<[u32; 3]> {
    v.iter().map(|x| x.map(f32::to_bits)).collect()
}

fn bits4(v: &[[f32; 4]]) -> Vec<[u32; 4]> {
    v.iter().map(|x| x.map(f32::to_bits)).collect()
}

#[test]
fn jitter_matches_across_schedules_and_keeps_the_other_columns() {
    let input = points();
    let serial = run(Parallelism::Serial, input.clone());
    for threads in [1, 2, 8] {
        let pooled = run(Parallelism::threads(threads), input.clone());
        assert_eq!(
            bits3(&pooled.position),
            bits3(&serial.position),
            "{threads} threads"
        );
        assert_eq!(
            bits4(&pooled.rotation),
            bits4(&serial.rotation),
            "{threads} threads"
        );
        assert_eq!(bits3(&pooled.scale), bits3(&serial.scale), "{threads} threads");
    }
    assert_eq!(serial.len(), N);
    assert_ne!(
        bits3(&serial.position),
        bits3(&input.position),
        "the node moved points"
    );
    assert_ne!(
        bits4(&serial.rotation),
        bits4(&input.rotation),
        "the node turned points"
    );
    assert_ne!(
        bits3(&serial.scale),
        bits3(&input.scale),
        "the node scaled points"
    );
    assert_eq!(bits4(&serial.color), bits4(&input.color));
    assert_eq!(bits3(&serial.bounds_min), bits3(&input.bounds_min));
    assert_eq!(bits3(&serial.bounds_max), bits3(&input.bounds_max));
    assert_eq!(serial.seed, input.seed);
    assert_eq!(
        serial.density.iter().map(|d| d.to_bits()).collect::<Vec<_>>(),
        input.density.iter().map(|d| d.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
        serial.hardness.iter().map(|d| d.to_bits()).collect::<Vec<_>>(),
        input.hardness.iter().map(|d| d.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn rows_past_the_shortest_read_column_keep_their_input_values() {
    let full = points();
    let whole = run(Parallelism::Serial, full.clone());
    let short = N - 4100; // the shortest column ends inside the second chunk
    let mut ragged = full.clone();
    ragged.rotation.truncate(short);
    for parallelism in [Parallelism::Serial, Parallelism::threads(8)] {
        let out = run(parallelism, ragged.clone());
        assert_eq!(out.position.len(), N);
        assert_eq!(out.rotation.len(), short);
        assert_eq!(out.scale.len(), N);
        // The zipped rows are jittered exactly as in the consistent set ...
        assert_eq!(bits3(&out.position[..short]), bits3(&whole.position[..short]));
        assert_eq!(bits4(&out.rotation), bits4(&whole.rotation[..short]));
        assert_eq!(bits3(&out.scale[..short]), bits3(&whole.scale[..short]));
        // ... and the rest keep their input values.
        assert_eq!(bits3(&out.position[short..]), bits3(&full.position[short..]));
        assert_eq!(bits3(&out.scale[short..]), bits3(&full.scale[short..]));
    }
}

/// The finiteness check reads only the columns jitter writes: a result that overflows `f32` still
/// fails the run with `attr_nan` naming the column, on every schedule.
#[test]
fn an_overflowing_jitter_still_fails_with_attr_nan() {
    let compiled = compile(&graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node(
                    "points.jitter",
                    &[("offset", "[(1.0e39, 0.0, 0.0), (1.0e39, 0.0, 0.0)]")],
                ),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    ));
    for parallelism in [Parallelism::Serial, Parallelism::threads(2)] {
        let frame = Frame {
            graph_inputs: [("in".to_string(), Bundle::one(DataItem::points(points())))]
                .into_iter()
                .collect(),
            ..Frame::default()
        };
        let result = Evaluator::new().with_parallelism(parallelism).run_frame(
            &compiled,
            &RunInputs::new(Arc::new(StaticSnapshot::default()), 11),
            &CancelToken::new(),
            &frame,
        );
        let Err(err) = result else {
            panic!("an overflowing offset must fail the run");
        };
        assert_eq!(err.code(), "attr_nan", "{err}");
        assert!(err.to_string().contains("position"), "{err}");
    }
}
