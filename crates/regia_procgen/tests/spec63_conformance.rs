// SPDX-License-Identifier: MPL-2.0
//! Spec 63 conformance (lane spec-25-62, 2026-11-10): the per-node validation rule of § 14 for every
//! registered node type, not only the ones with a ranged setting (`nodes_lib.rs` covers those with
//! `setting_out_of_range`).

mod common;

use common::{graph, node};
use regia_procgen::ProcNodeRegistry;
use regia_procgen::nodes::SettingType;

/// § 14 "a validation test (one bad setting → its error code)" for every node type: an unknown
/// setting name is `unknown_setting` naming the node, and every typed setting given a value of the
/// wrong shape (a map, which no typed setting accepts) is `setting_type` naming the node and the
/// setting. Settings typed `Any` are checked by their node and are left out.
#[test]
fn every_node_type_refuses_an_unknown_setting_and_a_wrong_typed_one() {
    let reg = ProcNodeRegistry::with_builtins();
    let mut typed = 0usize;
    for kind in reg.iter() {
        let id = kind.id();
        let g = graph(vec![(7, node(id, &[("zz_not_a_setting", "1.0")]))], &[]);
        let e = g.compile(&reg).expect_err("unknown setting");
        let hit =
            e.0.iter()
                .find(|x| x.code == "unknown_setting")
                .unwrap_or_else(|| panic!("{id}: no unknown_setting in {:?}", e.0));
        assert_eq!(hit.node, Some(7), "{id}");
        assert!(hit.message.contains("zz_not_a_setting"), "{id}: {}", hit.message);

        for d in kind.settings() {
            if matches!(d.ty, SettingType::Any) {
                continue;
            }
            typed += 1;
            let g = graph(vec![(7, node(id, &[(d.name, r#"{ "wrong": 1.0 }"#)]))], &[]);
            let e = g.compile(&reg).expect_err("wrong type");
            let hit =
                e.0.iter()
                    .find(|x| x.code == "setting_type" && x.message.contains(d.name));
            assert!(
                hit.is_some_and(|h| h.node == Some(7)),
                "{id}.{} ({}): {:?}",
                d.name,
                d.ty.name(),
                e.0
            );
        }
    }
    assert!(typed > 100, "only {typed} typed settings checked");
}

// ---------------------------------------------------------------------------------------------
// Behavior tests for node types that had none (§§ 6-12), on small hand-built inputs.

use regia_procgen::{
    AttrColumn, AttrValue, Bundle, CancelToken, DataItem, Evaluator, Payload, Point, PointSet, RunError,
    RunInputs, RunResult, Table,
};

/// Runs `g` with `graph.input` pins fed by hand-built bundles.
fn run_in(g: &regia_procgen::ProcGraph, inputs: Vec<(&str, Bundle)>) -> Result<RunResult, RunError> {
    let frame = regia_procgen::exec::Frame {
        graph_inputs: inputs.into_iter().map(|(k, b)| (k.to_string(), b)).collect(),
        ..Default::default()
    };
    let ri = RunInputs::new(
        common::snapshot(common::box_shape([0.0; 3], [200.0; 3], 0.0), None),
        7,
    );
    Evaluator::new().run_frame(&common::compile(g), &ri, &CancelToken::new(), &frame)
}

/// `graph.input "in"` → the node under test (id 2, its own settings) → `graph.output` per output
/// pin (`out_pins[k]` goes to graph output `out_pins[k]`).
fn one_node(ty: &str, settings: &[(&str, &str)], out_pins: &[&str]) -> regia_procgen::ProcGraph {
    let mut nodes = vec![
        (1, node("graph.input", &[("pin", "\"in\"")])),
        (2, node(ty, settings)),
    ];
    let mut edges: Vec<(u32, &str, u32, &str)> = vec![(1, "out", 2, "in")];
    for (k, pin) in out_pins.iter().enumerate() {
        let id = 10 + k as u32;
        nodes.push((id, node("graph.output", &[("pin", &format!("\"{pin}\""))])));
        edges.push((2, pin, id, "in"));
    }
    graph(nodes, &edges)
}

fn bundle(r: &RunResult, pin: &str) -> Bundle {
    r.output.graph_outputs.get(pin).cloned().unwrap_or_default()
}

fn points(densities: &[f32]) -> PointSet {
    let mut p = PointSet::new();
    for (i, d) in densities.iter().enumerate() {
        p.push(Point {
            position: [i as f32, 0.0, 0.0],
            density: *d,
            ..Point::default()
        });
    }
    p
}

fn tagged(tags: &[&str], p: PointSet) -> DataItem {
    let mut item = DataItem::points(p);
    item.tags = tags.iter().map(|t| t.to_string()).collect();
    item
}

fn xs(b: &Bundle) -> Vec<f32> {
    b.0.iter()
        .filter_map(DataItem::as_points)
        .flat_map(|p| p.position.iter().map(|q| q[0]))
        .collect()
}

fn tag_sets(b: &Bundle) -> Vec<Vec<String>> {
    b.0.iter().map(|i| i.tags.iter().cloned().collect()).collect()
}

fn item_hash(i: &DataItem) -> [u8; 32] {
    Bundle::one(i.clone()).hash()
}

fn node_err(r: &Result<RunResult, RunError>) -> Option<String> {
    match r {
        Err(RunError::Node { code, .. }) => Some(code.clone()),
        _ => None,
    }
}

/// § 6 `points.bake_scale`: `bounds *= scale` component-wise and `scale = sign(scale)`, so a mirror
/// survives and the point's world box does not change.
#[test]
fn bake_scale_moves_scale_into_bounds_and_keeps_mirrors() {
    let mut p = PointSet::new();
    p.push(Point {
        position: [1.0, 2.0, 3.0],
        scale: [2.0, 3.0, -4.0],
        ..Point::default()
    });
    let before = p.world_aabb(0);
    let r = run_in(
        &one_node("points.bake_scale", &[], &["out"]),
        vec![("in", Bundle::one(DataItem::points(p)))],
    )
    .expect("run");
    let b = bundle(&r, "out");
    let q = b.0[0].as_points().expect("points");
    assert_eq!(q.scale[0], [1.0, 1.0, -1.0], "sign kept");
    assert_eq!(q.bounds_min[0], [-1.0, -1.5, 2.0]);
    assert_eq!(q.bounds_max[0], [1.0, 1.5, -2.0]);
    assert_eq!(q.world_aabb(0), before, "the world box is unchanged");
}

/// § 7 `filter.compare` against a constant, column 0 of a one-row table on `rhs`, the region value
/// of `rhs`, and another attribute (`rhs_attr`); order is kept.
#[test]
fn filter_compare_constant_table_region_and_rhs_attr() {
    let input = || Bundle::one(DataItem::points(points(&[0.2, 0.5, 0.9, 0.7])));
    // Constant, default op `ge` and value 0.5.
    let r = run_in(
        &one_node("filter.compare", &[], &["inside", "outside"]),
        vec![("in", input())],
    )
    .expect("constant");
    assert_eq!(xs(&bundle(&r, "inside")), vec![1.0, 2.0, 3.0]);
    assert_eq!(xs(&bundle(&r, "outside")), vec![0.0]);

    // Table: `lt` column 0 of a one-row table (0.6).
    let with_rhs = |settings: &[(&str, &str)]| {
        let mut g = one_node("filter.compare", settings, &["inside", "outside"]);
        g.nodes.insert(3, node("graph.input", &[("pin", "\"rhs\"")]));
        g.edges.push(regia_procgen::Edge::new(3, "out", 2, "rhs"));
        g
    };
    let table = Table::one_row([("limit".to_string(), AttrValue::Float(0.6))]);
    let r = run_in(
        &with_rhs(&[("op", "lt"), ("rhs_from", "table")]),
        vec![
            ("in", input()),
            ("rhs", Bundle::one(DataItem::new(Payload::Table(table)))),
        ],
    )
    .expect("table");
    assert_eq!(xs(&bundle(&r, "inside")), vec![0.0, 1.0]);

    // Region: a box over x ∈ [1.5, 2.5] has value 1 there, 0 elsewhere; `ge` keeps density ≥ value,
    // so only x = 2 (density 0.9 < 1) goes outside.
    let shape = common::box_shape([2.0, 0.0, 0.0], [0.5, 1.0, 0.5], 0.0);
    let r = run_in(
        &with_rhs(&[("rhs_from", "region")]),
        vec![
            ("in", input()),
            ("rhs", Bundle::one(DataItem::new(Payload::Shape(shape)))),
        ],
    )
    .expect("region");
    assert_eq!(xs(&bundle(&r, "inside")), vec![0.0, 1.0, 3.0]);
    assert_eq!(xs(&bundle(&r, "outside")), vec![2.0]);

    // rhs_attr: density le w, element by element.
    let mut p = points(&[0.2, 0.5, 0.9, 0.7]);
    p.attrs
        .insert("w".into(), AttrColumn::Float(vec![0.1, 0.5, 1.0, 0.0].into()));
    let r = run_in(
        &one_node(
            "filter.compare",
            &[("op", "le"), ("rhs_attr", "\"w\"")],
            &["inside", "outside"],
        ),
        vec![("in", Bundle::one(DataItem::points(p)))],
    )
    .expect("rhs_attr");
    assert_eq!(xs(&bundle(&r, "inside")), vec![1.0, 2.0]);
}

/// Spec 62 § 3.5 through `filter.compare`: a vector attribute read as a scalar is
/// `attr_type_mismatch` unless a component suffix picks one part (before this lane the node read
/// component 0 silently).
#[test]
fn filter_compare_refuses_a_vector_without_a_suffix() {
    let input = || Bundle::one(DataItem::points(points(&[1.0, 1.0, 1.0])));
    let r = run_in(
        &one_node(
            "filter.compare",
            &[("attr", "\"position\""), ("value", "1.0")],
            &["inside"],
        ),
        vec![("in", input())],
    );
    assert_eq!(node_err(&r).as_deref(), Some("attr_type_mismatch"), "{r:?}");
    let r = run_in(
        &one_node(
            "filter.compare",
            &[("attr", "\"position:x\""), ("value", "1.0")],
            &["inside"],
        ),
        vec![("in", input())],
    )
    .expect("suffix");
    assert_eq!(xs(&bundle(&r, "inside")), vec![1.0, 2.0]);
    let r = run_in(
        &one_node("filter.compare", &[("rhs_attr", "\"position\"")], &["inside"]),
        vec![("in", input())],
    );
    assert_eq!(node_err(&r).as_deref(), Some("attr_type_mismatch"), "{r:?}");
}

/// § 8 `items.tag` edits in the order rename, remove, keep_only (when not empty), add.
#[test]
fn items_tag_applies_rename_remove_keep_only_add_in_order() {
    let input = || {
        Bundle(vec![
            tagged(&["a", "c"], points(&[1.0])),
            tagged(&["b", "x"], points(&[1.0])),
        ])
    };
    // Rename before remove: `a` renamed to `x` is then removed with the old `x`.
    let r = run_in(
        &one_node(
            "items.tag",
            &[("rename", "[(\"a\", \"x\")]"), ("remove", "[\"x\"]")],
            &["out"],
        ),
        vec![("in", input())],
    )
    .expect("run");
    assert_eq!(
        tag_sets(&bundle(&r, "out")),
        vec![vec!["c".to_string()], vec!["b".to_string()]]
    );
    // keep_only before add: `z` is added after the filter.
    let r = run_in(
        &one_node(
            "items.tag",
            &[("keep_only", "[\"c\", \"b\"]"), ("add", "[\"z\"]")],
            &["out"],
        ),
        vec![("in", input())],
    )
    .expect("run");
    assert_eq!(
        tag_sets(&bundle(&r, "out")),
        vec![
            vec!["c".to_string(), "z".to_string()],
            vec!["b".to_string(), "z".to_string()]
        ]
    );
}

/// § 8 `items.by_tag` (any, all), `items.by_kind`, `items.by_attr` (element and item scope) and the
/// `items.by_index` node: whole items are routed, input order is kept on both outputs.
#[test]
fn item_routing_nodes_keep_input_order() {
    let items = || {
        let mut with_w = points(&[1.0]);
        with_w
            .attrs
            .insert("w".into(), AttrColumn::Float(vec![1.0].into()));
        let mut scoped = tagged(&[], points(&[1.0]));
        scoped.scope.insert("k".into(), AttrValue::Int(3));
        Bundle(vec![
            tagged(&["a"], points(&[1.0])),
            tagged(&["b"], with_w),
            tagged(&["a", "b"], points(&[1.0])),
            scoped,
            DataItem::new(Payload::Table(Table::one_row([(
                "n".to_string(),
                AttrValue::Int(1),
            )]))),
        ])
    };
    let route = |ty: &str, settings: &[(&str, &str)]| {
        let r = run_in(
            &one_node(ty, settings, &["inside", "outside"]),
            vec![("in", items())],
        )
        .expect("run");
        let kinds = |b: &Bundle| -> Vec<usize> {
            let all = items();
            b.0.iter()
                .map(|i| {
                    all.0
                        .iter()
                        .position(|j| item_hash(j) == item_hash(i))
                        .expect("an input item")
                })
                .collect()
        };
        (kinds(&bundle(&r, "inside")), kinds(&bundle(&r, "outside")), r)
    };
    let (i, o, _) = route("items.by_tag", &[("tags", "[\"a\", \"b\"]")]);
    assert_eq!((i, o), (vec![0, 1, 2], vec![3, 4]));
    let (i, o, _) = route("items.by_tag", &[("tags", "[\"a\", \"b\"]"), ("all", "true")]);
    assert_eq!((i, o), (vec![2], vec![0, 1, 3, 4]));
    let (i, o, _) = route("items.by_kind", &[("kinds", "[\"Table\"]")]);
    assert_eq!((i, o), (vec![4], vec![0, 1, 2, 3]));
    let (i, _, _) = route("items.by_attr", &[("name", "\"w\"")]);
    assert_eq!(i, vec![1]);
    let (i, _, _) = route("items.by_attr", &[("name", "\"k\""), ("scope", "item")]);
    assert_eq!(i, vec![3]);
    let (i, o, r) = route("items.by_index", &[("select", "\"3, 1, -1, 9\"")]);
    assert_eq!(
        (i, o),
        (vec![1, 3, 4], vec![0, 2]),
        "original order, not list order"
    );
    let line = r.report.nodes.iter().find(|n| n.id == 2).expect("report");
    assert!(
        line.warnings.iter().any(|(c, _)| c == "index_ignored"),
        "9 is out of range: {:?}",
        line.warnings
    );
}

/// § 8 `items.bounds`: one table row per item with its world AABB as `min` and `max`.
#[test]
fn items_bounds_writes_one_row_per_item() {
    let mut a = PointSet::new();
    a.push(Point {
        position: [1.0, 0.0, 0.0],
        ..Point::default()
    });
    a.push(Point {
        position: [3.0, 2.0, -1.0],
        scale: [2.0, 2.0, 2.0],
        ..Point::default()
    });
    let b = points(&[1.0]);
    let r = run_in(
        &one_node("items.bounds", &[], &["out"]),
        vec![("in", Bundle(vec![DataItem::points(a), DataItem::points(b)]))],
    )
    .expect("run");
    let out = bundle(&r, "out");
    let Payload::Table(t) = &out.0[0].payload else {
        panic!("a table: {out:?}");
    };
    assert_eq!(t.rows, 2);
    assert_eq!(
        t.column("min").and_then(|c| c.get(0)),
        Some(AttrValue::Vec3([0.5, -0.5, -2.0]))
    );
    assert_eq!(
        t.column("max").and_then(|c| c.get(0)),
        Some(AttrValue::Vec3([4.0, 3.0, 0.5]))
    );
    assert_eq!(
        t.column("min").and_then(|c| c.get(1)),
        Some(AttrValue::Vec3([-0.5, -0.5, -0.5]))
    );
}

/// § 8 `items.sort`: sorts elements by the keys; stable, so ties keep input order, also when
/// descending.
#[test]
fn items_sort_is_stable_both_ways() {
    let input = || {
        let mut p = points(&[1.0, 1.0, 1.0, 1.0]);
        p.attrs
            .insert("k".into(), AttrColumn::Float(vec![3.0, 1.0, 2.0, 1.0].into()));
        Bundle::one(DataItem::points(p))
    };
    let r = run_in(
        &one_node("items.sort", &[("by", "[\"k\"]")], &["out"]),
        vec![("in", input())],
    )
    .expect("run");
    assert_eq!(xs(&bundle(&r, "out")), vec![1.0, 3.0, 2.0, 0.0]);
    let r = run_in(
        &one_node(
            "items.sort",
            &[("by", "[\"k\"]"), ("descending", "true")],
            &["out"],
        ),
        vec![("in", input())],
    )
    .expect("run");
    assert_eq!(
        xs(&bundle(&r, "out")),
        vec![0.0, 2.0, 1.0, 3.0],
        "ties keep input order"
    );
}

/// § 9 `attr.rename` (`attr_exists` unless `overwrite`, reserved targets refused) and `attr.remove`
/// (`drop`, `keep`; built-ins cannot be removed).
#[test]
fn attr_rename_and_remove() {
    let input = || {
        let mut p = points(&[0.25, 0.75]);
        p.attrs
            .insert("u".into(), AttrColumn::Float(vec![1.0, 2.0].into()));
        p.attrs
            .insert("v".into(), AttrColumn::Float(vec![3.0, 4.0].into()));
        p.attrs
            .insert("w".into(), AttrColumn::Float(vec![5.0, 6.0].into()));
        Bundle::one(DataItem::points(p))
    };
    let names = |r: &RunResult| -> Vec<String> {
        bundle(r, "out").0[0]
            .as_points()
            .expect("points")
            .attrs
            .keys()
            .cloned()
            .collect()
    };
    let rename = |settings: &[(&str, &str)]| {
        run_in(
            &one_node("attr.rename", settings, &["out"]),
            vec![("in", input())],
        )
    };
    let r = rename(&[("from", "\"u\""), ("to", "\"x\"")]).expect("rename");
    assert_eq!(names(&r), vec!["v", "w", "x"]);
    let p = bundle(&r, "out");
    assert_eq!(
        p.0[0].as_points().expect("points").attrs["x"].get(1),
        Some(AttrValue::Float(2.0))
    );
    assert_eq!(
        node_err(&rename(&[("from", "\"u\""), ("to", "\"v\"")])).as_deref(),
        Some("attr_exists")
    );
    let r = rename(&[("from", "\"u\""), ("to", "\"v\""), ("overwrite", "true")]).expect("overwrite");
    assert_eq!(names(&r), vec!["v", "w"]);
    assert_eq!(
        node_err(&rename(&[("from", "\"u\""), ("to", "\"_u\"")])).as_deref(),
        Some("attr_reserved_name")
    );

    let remove = |settings: &[(&str, &str)]| {
        run_in(
            &one_node("attr.remove", settings, &["out"]),
            vec![("in", input())],
        )
        .expect("remove")
    };
    assert_eq!(names(&remove(&[("drop", "[\"u\", \"w\"]")])), vec!["v"]);
    assert_eq!(names(&remove(&[("keep", "[\"w\"]")])), vec!["w"]);
    let r = remove(&[("drop", "[\"density\"]")]);
    assert_eq!(
        bundle(&r, "out").0[0]
            .as_points()
            .expect("points")
            .density
            .to_vec(),
        vec![0.25, 0.75],
        "a built-in is not removable"
    );
}

/// § 9 `table.from_points`: one row per point with the custom attributes; built-ins only when
/// listed in `include`; an item with no custom attributes gives an empty table.
#[test]
fn table_from_points_rows_columns_and_include() {
    let mut p = points(&[0.25, 0.75]);
    p.attrs
        .insert("w".into(), AttrColumn::Float(vec![5.0, 6.0].into()));
    let input = || {
        Bundle(vec![
            DataItem::points(p.clone()),
            DataItem::points(points(&[1.0])),
        ])
    };
    let r = run_in(
        &one_node("table.from_points", &[], &["out"]),
        vec![("in", input())],
    )
    .expect("run");
    let out = bundle(&r, "out");
    let tables: Vec<&Table> = out
        .0
        .iter()
        .map(|i| match &i.payload {
            Payload::Table(t) => t,
            other => panic!("a table: {other:?}"),
        })
        .collect();
    assert_eq!(tables.len(), 2, "one result per item");
    assert_eq!(tables[0].rows, 2);
    assert_eq!(
        tables[0]
            .columns
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        vec!["w"]
    );
    assert_eq!(tables[1].rows, 0, "no custom attributes: an empty table");
    let r = run_in(
        &one_node("table.from_points", &[("include", "[\"density\"]")], &["out"]),
        vec![("in", input())],
    )
    .expect("run");
    let out = bundle(&r, "out");
    let Payload::Table(t) = &out.0[0].payload else {
        panic!("a table");
    };
    assert_eq!(
        t.column("density").and_then(|c| c.get(1)),
        Some(AttrValue::Float(0.75))
    );
    assert!(t.column("w").is_some());
}

/// § 10 `flow.pick`: forwards the input of the first case equal to the key, else `default`.
#[test]
fn flow_pick_forwards_the_matching_case_or_default() {
    let pick = |key: &str| {
        let g = graph(
            vec![
                (1, node("graph.input", &[("pin", "\"a\"")])),
                (2, node("graph.input", &[("pin", "\"b\"")])),
                (3, node("graph.input", &[("pin", "\"d\"")])),
                (4, node("flow.pick", &[("key", key), ("cases", "[1, 2]")])),
                (5, node("graph.output", &[("pin", "\"out\"")])),
            ],
            &[
                (1, "out", 4, "case_0"),
                (2, "out", 4, "case_1"),
                (3, "out", 4, "default"),
                (4, "out", 5, "in"),
            ],
        );
        let one = |x: f32| {
            let mut p = PointSet::new();
            p.push(Point {
                position: [x, 0.0, 0.0],
                ..Point::default()
            });
            Bundle::one(DataItem::points(p))
        };
        let r = run_in(&g, vec![("a", one(1.0)), ("b", one(2.0)), ("d", one(9.0))]).expect("run");
        xs(&bundle(&r, "out"))
    };
    assert_eq!(pick("2"), vec![2.0]);
    assert_eq!(pick("1"), vec![1.0]);
    assert_eq!(pick("5"), vec![9.0], "no case matches: default");
}

/// § 12 `debug.log` and `debug.show` pass their input through unchanged (same bundle hash).
#[test]
fn debug_log_and_show_pass_data_through() {
    let mut p = points(&[0.1, 0.9]);
    p.attrs
        .insert("w".into(), AttrColumn::Float(vec![1.0, 2.0].into()));
    let input = Bundle(vec![tagged(&["t"], p)]);
    for ty in ["debug.log", "debug.show"] {
        let r = run_in(&one_node(ty, &[], &["out"]), vec![("in", input.clone())]).expect("run");
        assert_eq!(bundle(&r, "out").hash(), input.hash(), "{ty}");
    }
}

/// § 12 `debug.log` placeholders: `{len}`, `{items}` and `{attr:<sel>}` (the first element of the
/// first item; names as text; `-` without an element; `?` when unreadable).
#[test]
fn debug_log_fills_len_items_and_attr_placeholders() {
    use regia_procgen::nodes::debug_log_message;
    let mut p = points(&[0.25, 0.5]);
    p.attrs
        .insert("w".into(), AttrColumn::Float(vec![1.5, 2.0].into()));
    p.attrs.insert(
        "kind".into(),
        AttrColumn::Name(vec!["rock".to_string(), "tree".to_string()].into()),
    );
    let mut item = DataItem::points(p);
    item.scope
        .insert("biome".into(), AttrValue::Name("alpine".into()));
    let data = Bundle(vec![item, DataItem::points(points(&[1.0]))]);
    assert_eq!(
        debug_log_message("{items} items, {len} elements", &data),
        "2 items, 3 elements"
    );
    assert_eq!(
        debug_log_message(
            "w={attr:w} d={attr:density} p={attr:position} x={attr:position:x} k={attr:kind} b={attr:item.biome} m={attr:missing}",
            &data
        ),
        "w=1.5 d=0.25 p=0,0,0 x=0 k=rock b=alpine m=?"
    );
    assert_eq!(debug_log_message("{attr:w}", &Bundle::empty()), "-");
    assert_eq!(
        debug_log_message("open {attr:w", &data),
        "open {attr:w",
        "an unclosed placeholder stays"
    );
}

/// § 9 `attr.math` edge rules: floored `mod`, `round` half away from zero, `frac`, `sign`, `clamp`,
/// `lerp`, division by zero yielding 0 with one `div_by_zero` warning, `normalize` of a vector
/// shorter than 1e-12 yielding 0.
#[test]
fn attr_math_edge_rules() {
    let run_op = |op: &str, b: &str, c: &str| -> (Vec<f64>, usize) {
        let mut p = points(&[1.0, 1.0, 1.0, 1.0]);
        p.attrs
            .insert("x".into(), AttrColumn::Float(vec![-1.0, 2.5, -2.5, 0.0].into()));
        let r = run_in(
            &one_node(
                "attr.math",
                &[
                    ("op", op),
                    ("a", "\"x\""),
                    ("b", b),
                    ("c", c),
                    ("target", "\"r\""),
                ],
                &["out"],
            ),
            vec![("in", Bundle::one(DataItem::points(p)))],
        )
        .unwrap_or_else(|e| panic!("{op}: {e}"));
        let out = bundle(&r, "out");
        let col = &out.0[0].as_points().expect("points").attrs["r"];
        let vals = (0..4)
            .map(|i| match col.get(i) {
                Some(AttrValue::Float(v)) => v,
                other => panic!("{op}: {other:?}"),
            })
            .collect();
        let warns = r.report.nodes.iter().find(|n| n.id == 2).map_or(0, |n| {
            n.warnings.iter().filter(|(c, _)| c == "div_by_zero").count()
        });
        (vals, warns)
    };
    assert_eq!(run_op("mod", "3.0", "0.0").0, vec![2.0, 2.5, 0.5, 0.0], "floored");
    assert_eq!(
        run_op("round", "0.0", "0.0").0,
        vec![-1.0, 3.0, -3.0, 0.0],
        "half away from zero"
    );
    assert_eq!(run_op("frac", "0.0", "0.0").0, vec![0.0, 0.5, 0.5, 0.0]);
    assert_eq!(run_op("sign", "0.0", "0.0").0, vec![-1.0, 1.0, -1.0, 0.0]);
    assert_eq!(run_op("clamp", "-2.0", "2.0").0, vec![-1.0, 2.0, -2.0, 0.0]);
    assert_eq!(run_op("lerp", "1.0", "0.5").0, vec![0.0, 1.75, -0.75, 0.5]);
    let (div, warns) = run_op("div", "0.0", "0.0");
    assert_eq!(div, vec![0.0; 4], "division by zero yields 0");
    assert_eq!(warns, 1, "one div_by_zero warning per node");
    let (m, warns) = run_op("mod", "0.0", "0.0");
    assert_eq!((m, warns), (vec![0.0; 4], 1));

    // normalize: a zero vector gives 0, others unit length.
    let mut p = points(&[1.0, 1.0]);
    p.attrs.insert(
        "v".into(),
        AttrColumn::Vec3(vec![[0.0, 0.0, 0.0], [3.0, 0.0, 4.0]].into()),
    );
    let r = run_in(
        &one_node(
            "attr.math",
            &[("op", "normalize"), ("a", "\"v\""), ("target", "\"n\"")],
            &["out"],
        ),
        vec![("in", Bundle::one(DataItem::points(p)))],
    )
    .expect("normalize");
    let out = bundle(&r, "out");
    let n = &out.0[0].as_points().expect("points").attrs["n"];
    assert_eq!(n.get(0), Some(AttrValue::Vec3([0.0, 0.0, 0.0])));
    assert_eq!(n.get(1), Some(AttrValue::Vec3([0.6, 0.0, 0.8])));
}

/// A `count` × `count` grid of points at `step` meters with density 1 (and the given seeds from
/// their positions, as samplers write them).
fn grid_points(count: usize, step: f32) -> PointSet {
    let mut p = PointSet::new();
    for z in 0..count {
        for x in 0..count {
            let position = [x as f32 * step, 0.0, z as f32 * step];
            p.push(Point {
                position,
                seed: regia_procgen::seeds::point_seed(position, 0),
                ..Point::default()
            });
        }
    }
    p
}

/// § 6 `points.bounds`: `set` writes `min`/`max`, `scale` multiplies the box by `factor`.
#[test]
fn points_bounds_set_and_scale() {
    let input = || Bundle::one(DataItem::points(points(&[1.0, 1.0])));
    let r = run_in(
        &one_node(
            "points.bounds",
            &[("min", "(-1.0, 0.0, -2.0)"), ("max", "(1.0, 3.0, 2.0)")],
            &["out"],
        ),
        vec![("in", input())],
    )
    .expect("set");
    let out = bundle(&r, "out");
    let p = out.0[0].as_points().expect("points");
    assert!(p.bounds_min.iter().all(|b| *b == [-1.0, 0.0, -2.0]));
    assert!(p.bounds_max.iter().all(|b| *b == [1.0, 3.0, 2.0]));
    let r = run_in(
        &one_node(
            "points.bounds",
            &[("mode", "scale"), ("factor", "(2.0, 4.0, 1.0)")],
            &["out"],
        ),
        vec![("in", input())],
    )
    .expect("scale");
    let out = bundle(&r, "out");
    let p = out.0[0].as_points().expect("points");
    assert_eq!(p.bounds_min[0], [-1.0, -2.0, -0.5]);
    assert_eq!(p.bounds_max[0], [1.0, 2.0, 0.5]);
}

/// § 5 `density.curve`: piecewise linear between keys, constant beyond the end keys.
#[test]
fn density_curve_is_piecewise_linear_and_constant_past_the_ends() {
    let r = run_in(
        &one_node(
            "density.curve",
            &[("curve", "[(0.2, 0.1), (0.6, 0.9), (0.8, 0.5)]")],
            &["out"],
        ),
        vec![(
            "in",
            Bundle::one(DataItem::points(points(&[0.0, 0.2, 0.4, 0.7, 0.8, 1.0]))),
        )],
    )
    .expect("run");
    let out = bundle(&r, "out");
    let d: Vec<f32> = out.0[0].as_points().expect("points").density.to_vec();
    let want = [0.1, 0.1, 0.5, 0.7, 0.5, 0.5];
    for (got, want) in d.iter().zip(want) {
        assert!((got - want).abs() < 1e-6, "{d:?}");
    }
}

/// § 5 `density.noise`: the noise is normalized to [0, 1] and remapped to `[min, max]`; `set`
/// writes it, `mul` multiplies the density; the same seed gives the same values and they vary over
/// space.
#[test]
fn density_noise_stays_in_its_range_and_combines_by_mode() {
    let input = |d: f32| {
        let mut p = grid_points(32, 3.0);
        p.density.make_mut().iter_mut().for_each(|x| *x = d);
        Bundle::one(DataItem::points(p))
    };
    let noise = |settings: &[(&str, &str)], d: f32| -> Vec<f32> {
        let r = run_in(
            &one_node("density.noise", settings, &["out"]),
            vec![("in", input(d))],
        )
        .expect("run");
        bundle(&r, "out").0[0]
            .as_points()
            .expect("points")
            .density
            .to_vec()
    };
    let set = noise(&[("mode", "set"), ("min", "0.25"), ("max", "0.75")], 1.0);
    assert!(set.iter().all(|v| (0.25..=0.75).contains(v)), "remapped range");
    let (lo, hi) = set
        .iter()
        .fold((1.0f32, 0.0f32), |(a, b), v| (a.min(*v), b.max(*v)));
    assert!(hi - lo > 0.1, "the noise varies over 96 m: {lo}..{hi}");
    assert_eq!(
        set,
        noise(&[("mode", "set"), ("min", "0.25"), ("max", "0.75")], 1.0),
        "repeatable"
    );
    let mul = noise(&[("mode", "mul"), ("min", "0.25"), ("max", "0.75")], 0.5);
    for (m, s) in mul.iter().zip(&set) {
        assert!((m - 0.5 * s).abs() < 1e-6, "mul halves the set value: {m} vs {s}");
    }
}

/// § 6 `points.keep_random`: each point is kept when its draw is below `ratio`, so the count is
/// near `ratio · n` but not exact; 0 keeps none and 1 keeps all; the kept set is repeatable.
#[test]
fn keep_random_keeps_about_the_ratio() {
    let keep = |ratio: &str| {
        let r = run_in(
            &one_node("points.keep_random", &[("ratio", ratio)], &["out"]),
            vec![("in", Bundle::one(DataItem::points(grid_points(100, 1.0))))],
        )
        .expect("run");
        bundle(&r, "out").0[0]
            .as_points()
            .expect("points")
            .position
            .to_vec()
    };
    let half = keep("0.5");
    assert!((4_700..=5_300).contains(&half.len()), "kept {}", half.len());
    assert_eq!(half, keep("0.5"), "repeatable");
    assert!(keep("0.0").is_empty());
    assert_eq!(keep("1.0").len(), 10_000);
}

/// § 14 cases 4, 7 and 12 ask for **recorded** counts: the ranges in `nodes_lib.rs` prove the
/// distribution, these exact values pin the draws (slot 0 of each node) so a change to a node's
/// random stream shows up here. Same graphs, seeds and snapshot as `case_04`, `case_07` and
/// `case_12`.
#[test]
fn recorded_counts_of_cases_4_7_and_12() {
    let snap = || common::snapshot(common::box_shape([0.0; 3], [10.0; 3], 0.0), None);
    // Case 4: 100 000 points of density 0.3 through `density.realize random`, seed 5.
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(400.0, 1.0, 250.0)")])),
            (
                2,
                node("attr.set", &[("target", "\"density\""), ("value", "0.3")]),
            ),
            (3, node("density.realize", &[("mode", "random")])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let kept = common::out_points(&common::run(&g, snap(), 5), "out").len();

    // Case 7: four weight-1 meshes and a weight-0 one over 40 000 points, seed 4.
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
    let r = common::run(&g, snap(), 4);
    let meshes: Vec<(String, usize)> = r
        .output
        .instance_sets
        .iter()
        .map(|s| (s.mesh.to_string(), s.instances.len()))
        .collect();

    // Case 12: weighted multi-match lookup over 10 000 elements, seed 6.
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
    let p = common::out_points(&common::run(&g, snap(), 6), "out");
    let Some(AttrColumn::Name(v)) = p.attrs.get("variant") else {
        panic!("variant column");
    };
    let variants: Vec<usize> = ["a", "b", "c"]
        .iter()
        .map(|x| v.iter().filter(|s| s.as_str() == *x).count())
        .collect();

    let got = format!("case 4: {kept}; case 7: {meshes:?}; case 12: {variants:?}");
    assert_eq!(got, RECORDED_COUNTS, "recorded counts changed");
}

/// The counts `recorded_counts_of_cases_4_7_and_12` recorded (2026-11-11, Linux x86_64).
const RECORDED_COUNTS: &str = "case 4: 30083; case 7: [(\"d\", 10037), (\"a\", 9923), (\"c\", 10118), (\"b\", 9922)]; case 12: [2468, 4997, 2535]";
