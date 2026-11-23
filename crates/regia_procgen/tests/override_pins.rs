// SPDX-License-Identifier: MPL-2.0
//! Override pins (spec 62 § 4.1, exposed by the editor's "show as pin", spec 67 § 2.1): an edge into
//! `@<setting>` carries a `Table`; row 0 of the setting's column (or the sole column) replaces the
//! setting for that run, winning over literal values and `Bind`.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, compile, graph, node, out_points, snapshot};
use regia_procgen::data::AttrValue;
use regia_procgen::exec::StaticSnapshot;
use regia_procgen::{CancelToken, Evaluator, ProcCache, ProcNodeRegistry, RunInputs, Table};

/// gen.grid (8 points) → points.keep_random → graph output `out`; in.table → `@ratio` when wired.
fn ratio_graph(wired: bool, pin: &str) -> regia_procgen::ProcGraph {
    let mut edges = vec![(1, "out", 2, "in"), (2, "out", 4, "in")];
    if wired {
        edges.push((3, "out", 2, pin));
    }
    graph(
        vec![
            (1, node("gen.grid", &[("count", "(4.0, 1.0, 2.0)")])),
            (2, node("points.keep_random", &[("ratio", "0.5")])),
            (3, node("in.table", &[("asset", "\"t.table.ron\"")])),
            (4, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &edges,
    )
}

fn snap_with(table: Table) -> Arc<StaticSnapshot> {
    let base = snapshot(box_shape([0.0; 3], [8.0, 2.0, 8.0], 0.0), None);
    let mut s = (*base).clone();
    s.tables = BTreeMap::from([("t.table.ron".to_string(), table)]);
    Arc::new(s)
}

fn ratio_table(r: f64) -> Table {
    Table::one_row([("ratio".to_string(), AttrValue::Float(r))])
}

fn count(
    g: &regia_procgen::ProcGraph,
    snap: Arc<StaticSnapshot>,
    cache: Option<&Arc<parking_lot::Mutex<ProcCache>>>,
) -> usize {
    let c = compile(g);
    let ev = match cache {
        Some(c) => Evaluator::with_cache(c.clone()),
        None => Evaluator::new(),
    };
    match ev.run(&c, &RunInputs::new(snap, 7), &CancelToken::new()) {
        Ok(r) => out_points(&r, "out").len(),
        Err(e) => panic!("run failed: {e}"),
    }
}

#[test]
fn an_override_pin_wins_over_the_literal_setting() {
    let unwired = count(&ratio_graph(false, "@ratio"), snap_with(ratio_table(0.0)), None);
    assert!(
        unwired > 0 && unwired < 8,
        "the literal 0.5 keeps some: {unwired}"
    );
    assert_eq!(
        count(&ratio_graph(true, "@ratio"), snap_with(ratio_table(0.0)), None),
        0
    );
    assert_eq!(
        count(&ratio_graph(true, "@ratio"), snap_with(ratio_table(1.0)), None),
        8
    );
    // A one-column table feeds the setting whatever its column is called.
    let sole = Table::one_row([("k".to_string(), AttrValue::Int(1))]);
    assert_eq!(
        count(&ratio_graph(true, "@ratio"), snap_with(sole), None),
        8,
        "Int converts to Float"
    );
}

#[test]
fn the_cache_keys_on_the_override_value() {
    let cache = Arc::new(parking_lot::Mutex::new(ProcCache::new(64 << 20)));
    let g = ratio_graph(true, "@ratio");
    assert_eq!(count(&g, snap_with(ratio_table(0.0)), Some(&cache)), 0);
    assert_eq!(
        count(&g, snap_with(ratio_table(1.0)), Some(&cache)),
        8,
        "a cached run is not reused"
    );
    assert_eq!(count(&g, snap_with(ratio_table(0.0)), Some(&cache)), 0);
}

#[test]
fn bad_override_pins_are_reported() {
    let registry = ProcNodeRegistry::with_builtins();
    let errs = ratio_graph(true, "@nope")
        .compile(&registry)
        .expect_err("no such setting");
    assert!(errs.has("unknown_pin"), "{errs:?}");
    // A table without the setting's column (and more than one column) fails the run, naming the node.
    let two = Table::one_row([
        ("a".to_string(), AttrValue::Float(1.0)),
        ("b".to_string(), AttrValue::Float(1.0)),
    ]);
    let c = compile(&ratio_graph(true, "@ratio"));
    let err = Evaluator::new()
        .run(&c, &RunInputs::new(snap_with(two), 7), &CancelToken::new())
        .expect_err("no column");
    assert_eq!(err.code(), "override_invalid");
    // A name that does not convert.
    let name = Table::one_row([("ratio".to_string(), AttrValue::Name("half".into()))]);
    let err = Evaluator::new()
        .run(&c, &RunInputs::new(snap_with(name), 7), &CancelToken::new())
        .expect_err("type");
    assert_eq!(err.code(), "override_invalid");
}

#[test]
fn node_pins_list_the_wired_override_pin() {
    let registry = ProcNodeRegistry::with_builtins();
    let g = ratio_graph(true, "@ratio");
    let pins = g.node_pins(2, &registry, &|_| None).expect("pins");
    assert_eq!(
        pins.inputs.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["in", "@ratio"]
    );
    let plain = ratio_graph(false, "@ratio")
        .node_pins(2, &registry, &|_| None)
        .expect("pins");
    assert_eq!(plain.inputs.len(), 1);
}
