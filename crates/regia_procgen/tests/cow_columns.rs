// SPDX-License-Identifier: MPL-2.0
//! Copy-on-write point columns (ADR-2731): a linear chain of in-place nodes stops copying its points,
//! a shared column is copied only when a node writes it, and the output bytes do not depend on who
//! held what. The copy counter is process-wide, so every test that reads it holds [`COUNTER`].

mod common;

use std::sync::{Arc, Mutex, MutexGuard};

use common::{compile, graph, node};
use regia_procgen::data::{AttrColumn, Column, Point, column_copy_stats};
use regia_procgen::exec::{Parallelism, StaticSnapshot};
use regia_procgen::{CancelToken, CompiledGraph, Evaluator, PointSet, ProcCache, ProcGraph, RunInputs};

static COUNTER: Mutex<()> = Mutex::new(());

fn counter() -> MutexGuard<'static, ()> {
    COUNTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cache(bytes: usize) -> Arc<parking_lot::Mutex<ProcCache>> {
    Arc::new(parking_lot::Mutex::new(ProcCache::new(bytes)))
}

fn inputs(keep_debug: bool) -> RunInputs {
    let mut i = RunInputs::new(Arc::new(StaticSnapshot::default()), 11);
    i.keep_debug = keep_debug;
    i
}

/// A grid and five in-place nodes in a line. Each of the five rewrites a column of its input.
fn chain() -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(100.0, 1.0, 100.0)"), ("step_m", "(0.5, 1.0, 0.5)")],
                ),
            ),
            (
                2,
                node("density.remap", &[("from", "[0.0, 1.0]"), ("to", "[0.2, 0.9]")]),
            ),
            (3, node("density.curve", &[("curve", "[(0.0, 0.1), (1.0, 0.8)]")])),
            (4, node("points.reseed", &[("mode", "\"index\"")])),
            (
                5,
                node(
                    "points.bounds",
                    &[("min", "(-0.2, -0.2, -0.2)"), ("max", "(0.2, 0.2, 0.2)")],
                ),
            ),
            (
                6,
                node(
                    "points.jitter",
                    &[("offset", "[(-0.1, 0.0, -0.1), (0.1, 0.0, 0.1)]")],
                ),
            ),
            (7, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
            (5, "out", 6, "in"),
            (6, "out", 7, "in"),
        ],
    )
}

fn run(ev: &Evaluator, c: &CompiledGraph, keep_debug: bool) -> (Vec<u8>, u64, u64) {
    let (copies0, bytes0) = column_copy_stats();
    let r = ev
        .run(c, &inputs(keep_debug), &CancelToken::new())
        .unwrap_or_else(|e| panic!("run: {e}"));
    let (copies1, bytes1) = column_copy_stats();
    (r.output_hash.to_vec(), copies1 - copies0, bytes1 - bytes0)
}

#[test]
fn a_five_node_chain_copies_at_most_one_column_on_every_schedule() {
    let _g = counter();
    let c = compile(&chain());
    let reference = run(&Evaluator::new().with_parallelism(Parallelism::Serial), &c, false);
    assert!(reference.1 <= 1, "serial run copied {} columns", reference.1);
    for threads in [None, Some(0), Some(1), Some(2), Some(8)] {
        let ev = Evaluator::new();
        let ev = match threads {
            Some(t) => ev.with_parallelism(Parallelism::threads(t)),
            None => ev,
        };
        let got = run(&ev, &c, false);
        assert_eq!(got.0, reference.0, "threads {threads:?} changed the output");
        assert!(got.1 <= 1, "threads {threads:?}: {} column copies", got.1);
    }
}

/// With a debug run every node's output stays alive, so each write must copy: the output must not
/// change, and that it copied at all proves nothing was written through a shared column.
#[test]
fn a_run_that_keeps_every_output_gets_the_same_bytes() {
    let _g = counter();
    let c = compile(&chain());
    let plain = run(&Evaluator::new().with_parallelism(Parallelism::Serial), &c, false);
    let kept = run(&Evaluator::new().with_parallelism(Parallelism::Serial), &c, true);
    assert_eq!(plain.0, kept.0);
    assert!(
        kept.1 >= 5,
        "five written columns were held elsewhere: {}",
        kept.1
    );
    let debug = Evaluator::new()
        .with_parallelism(Parallelism::Serial)
        .run(&c, &inputs(true), &CancelToken::new())
        .unwrap_or_else(|e| panic!("run: {e}"));
    let nodes = debug.debug.unwrap_or_else(|| panic!("debug outputs"));
    // Every step's density column keeps its own values: node 2 remapped 1.0 to 0.9, node 3 mapped
    // that through the curve to 0.73.
    let density = |id: u32| {
        nodes[&id][0].0[0]
            .as_points()
            .unwrap_or_else(|| panic!("points"))
            .density[0]
    };
    assert_eq!(density(1), 1.0);
    assert!((density(2) - 0.9).abs() < 1e-6, "{}", density(2));
    assert!((density(3) - 0.73).abs() < 1e-6, "{}", density(3));
}

/// A node cache keeps each output, so the next writer must copy what it writes. It copies only that:
/// the five nodes rewrite 1, 1, 1, 2 and 3 columns, not nine columns five times.
#[test]
fn a_cached_chain_copies_only_the_columns_it_writes() {
    let _g = counter();
    let c = compile(&chain());
    let plain = run(&Evaluator::new().with_parallelism(Parallelism::Serial), &c, false);
    let ev = Evaluator::with_cache(cache(1 << 30)).with_parallelism(Parallelism::Serial);
    let cold = run(&ev, &c, false);
    assert_eq!(cold.0, plain.0);
    assert!(cold.1 <= 8, "{} column copies with a cache", cold.1);
    // One full set is 92 bytes a point; the old run copied it for each of the five nodes.
    let whole_sets = 5 * 92 * 10_000u64;
    assert!(cold.2 * 2 < whole_sets, "{} bytes of {whole_sets}", cold.2);
    // Served from the cache: no node writes, nothing is copied.
    let warm = run(&ev, &c, false);
    assert_eq!(warm.0, plain.0);
    assert!(warm.1 <= 1, "{} column copies from a warm cache", warm.1);
}

/// One output read by two in-place nodes: the first reader shares it and copies what it writes, the
/// last reader takes it. Neither sees the other's writes, on any schedule.
#[test]
fn two_readers_of_one_output_do_not_see_each_others_writes() {
    let _g = counter();
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(50.0, 1.0, 50.0)")])),
            (
                2,
                node("density.remap", &[("from", "[0.0, 1.0]"), ("to", "[0.25, 0.25]")]),
            ),
            (
                3,
                node("density.remap", &[("from", "[0.0, 1.0]"), ("to", "[0.75, 0.75]")]),
            ),
            (4, node("out.points", &[("name", "\"low\"")])),
            (5, node("out.points", &[("name", "\"high\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (1, "out", 3, "in"),
            (2, "out", 4, "in"),
            (3, "out", 5, "in"),
        ],
    );
    let c = compile(&g);
    for t in [0, 1, 2, 8] {
        let r = Evaluator::new()
            .with_parallelism(Parallelism::threads(t))
            .run(&c, &inputs(false), &CancelToken::new())
            .unwrap_or_else(|e| panic!("run: {e}"));
        let d = |name: &str| r.output.points[name].density[0];
        assert_eq!(d("low"), 0.25, "threads {t}");
        assert_eq!(d("high"), 0.75, "threads {t}");
    }
}

#[test]
fn a_write_copies_only_the_shared_column() {
    let _g = counter();
    let mut a = PointSet::new();
    a.push(Point::default());
    a.attrs
        .insert("k".to_string(), AttrColumn::Float(vec![1.0].into()));
    let (c0, b0) = column_copy_stats();
    let mut b = a.clone();
    assert_eq!(column_copy_stats().0, c0, "a clone copies nothing");
    assert!(a.position.shares_with(&b.position));
    b.density[0] = 0.5;
    let (c1, b1) = column_copy_stats();
    assert_eq!((c1 - c0, b1 - b0), (1, 4), "one f32 column of one element");
    assert_eq!(a.density[0], 1.0, "the source keeps its value");
    assert!(!a.density.shares_with(&b.density));
    assert!(
        a.position.shares_with(&b.position),
        "the other columns stay shared"
    );
    if let Some(AttrColumn::Float(v)) = b.attrs.get_mut("k") {
        v[0] = 2.0;
    }
    assert_eq!(a.attrs["k"].get(0), Some(regia_procgen::AttrValue::Float(1.0)));
    // Unique columns write in place.
    let (c2, _) = column_copy_stats();
    b.density[0] = 0.25;
    assert_eq!(column_copy_stats().0, c2);
}

#[test]
fn a_column_reads_and_compares_as_its_elements() {
    let c: Column<f32> = vec![1.0, f32::NAN].into();
    let same = c.clone();
    assert!(c.shares_with(&same));
    assert_ne!(c, same, "NaN stays unequal even in one allocation");
    let d: Column<i32> = (0..3).collect();
    assert_eq!(d, vec![0, 1, 2]);
    assert_eq!(d, [0, 1, 2]);
    assert_eq!(d.iter().sum::<i32>(), 3);
    assert_eq!((&d).into_iter().count(), 3);
    assert_eq!(d.clone().into_vec(), vec![0, 1, 2]);
    let mut e = d.clone();
    e.extend([3]);
    assert_eq!((d.len(), e.len()), (3, 4));
    assert_eq!(Vec::from(e), vec![0, 1, 2, 3]);
}

/// Hand-run numbers: 1 M points through the five nodes, serial, with and without a cache. Prints
/// them; there is no budget in it (`cargo test --release --test cow_columns -- --ignored --nocapture`).
#[test]
#[ignore = "hand-run: prints before and after numbers for ADR-2731"]
fn million_point_chain_numbers() {
    let _g = counter();
    let g = chain();
    let mut g2 = g.clone();
    if let Some(n) = g2.nodes.get_mut(&1) {
        *n = node(
            "gen.grid",
            &[("count", "(1000.0, 1.0, 1000.0)"), ("step_m", "(0.5, 1.0, 0.5)")],
        );
    }
    let c = compile(&g2);
    let time = |label: &str, ev: &Evaluator| {
        let mut ms: Vec<f64> = Vec::new();
        let mut copies = (0, 0);
        for _ in 0..9 {
            let (c0, b0) = column_copy_stats();
            let r = ev
                .run(&c, &inputs(false), &CancelToken::new())
                .unwrap_or_else(|e| panic!("run: {e}"));
            let (c1, b1) = column_copy_stats();
            copies = (c1 - c0, b1 - b0);
            ms.push(r.report.nodes.iter().skip(1).map(|n| n.ms).sum());
        }
        ms.sort_by(f64::total_cmp);
        println!(
            "{label}: five nodes p50 {:.1} ms (min {:.1}, max {:.1}), last run copied {} columns, {} MB",
            ms[4],
            ms[0],
            ms[8],
            copies.0,
            copies.1 / 1_000_000
        );
    };
    time(
        "no cache, serial",
        &Evaluator::new().with_parallelism(Parallelism::Serial),
    );
    time(
        "no cache, 8 threads",
        &Evaluator::new().with_parallelism(Parallelism::threads(8)),
    );
    let cold = |label: &str| {
        // A fresh cache per evaluator: warm runs would measure the lookup.
        let ev = Evaluator::with_cache(cache(1 << 34)).with_parallelism(Parallelism::Serial);
        let (c0, b0) = column_copy_stats();
        let r = ev
            .run(&c, &inputs(false), &CancelToken::new())
            .unwrap_or_else(|e| panic!("run: {e}"));
        let (c1, b1) = column_copy_stats();
        let ms: f64 = r.report.nodes.iter().skip(1).map(|n| n.ms).sum();
        println!(
            "{label}: five nodes {ms:.1} ms, copied {} columns, {} MB",
            c1 - c0,
            (b1 - b0) / 1_000_000
        );
    };
    cold("cold cache, serial");
}
