// SPDX-License-Identifier: MPL-2.0
//! Regression tests for the procgen audit findings fixed in fix-audit-wave10c (ADR-2391):
//! spline and shape generator bounds (M17), deterministic terrain choice (L13), unified setting
//! codes (L7), newer node settings (L3), mistyped settings (L8), reserved table columns (L9),
//! `points.flat_only` without ground (L10) and mask rectangles (L11).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, compile, graph, node, run_with, snapshot};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::data::{AttrColumn, MaskData, Table};
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator,
    ProcGraphSource, ProcOutput, generator_bounds, run_now,
};
use regia_procgen::{NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs};
use regia_scene::components::EntityGuid;
use regia_scene::shape::{Shape, ShapeKind};
use regia_scene::spline::Spline;
use regia_scene::{GlobalTransform, Transform};

/// `in.self → sample.ground → out.points`.
fn ground_graph() -> String {
    graph(
        vec![
            (1, node("in.self", &[])),
            (2, node("sample.ground", &[("spacing_m", "8.0")])),
            (3, node("out.points", &[])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in")],
    )
    .to_ron()
}

/// `gen.one → out.points` (reads nothing).
fn one_graph() -> String {
    graph(
        vec![(1, node("gen.one", &[])), (2, node("out.points", &[]))],
        &[(1, "out", 2, "in")],
    )
    .to_ron()
}

fn world_app() -> App {
    let files: BTreeMap<String, String> = [
        ("ground.pgraph.ron".to_string(), ground_graph()),
        ("one.pgraph.ron".to_string(), one_graph()),
    ]
    .into();
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app
}

fn x_extent(world: &World, e: Entity) -> f32 {
    let (lo, hi) = generator_bounds(world, e).world_aabb();
    hi[0] - lo[0]
}

/// M17: `ProcBounds::FromSpline` covers the entity's spline plus the margin, not a unit box.
#[test]
fn m17_spline_bounds_cover_the_spline_plus_the_margin() {
    let mut app = world_app();
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromSpline { margin_m: 2.0 },
                ..ProcGenerator::new("one.pgraph.ron")
            },
            Spline::through([Vec3::ZERO, Vec3::new(100.0, 0.0, 0.0)]),
            EntityGuid(Guid(41)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let w = x_extent(app.world(), e);
    assert!(w >= 104.0 - 1e-3, "spline bounds are {w} m wide");
    assert!(w < 120.0, "spline bounds are {w} m wide");
}

/// M17: `ProcBounds::FromShape` follows the entity's `Shape`.
#[test]
fn m17_shape_bounds_follow_the_shape() {
    let mut app = world_app();
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromShape,
                ..ProcGenerator::new("one.pgraph.ron")
            },
            Shape::new(ShapeKind::Box {
                half_extents: Vec3::new(30.0, 5.0, 30.0),
            }),
            EntityGuid(Guid(42)),
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            GlobalTransform::default(),
        ))
        .id();
    let (lo, hi) = generator_bounds(app.world(), e).world_aabb();
    assert!((hi[0] - lo[0] - 60.0).abs() < 1e-3, "{lo:?} {hi:?}");
    assert!((lo[0] + 20.0).abs() < 1e-3, "{lo:?} {hi:?}");
}

/// M17: a spline or shape bound on an entity without that component refuses to run instead of
/// scattering over a unit box at the origin.
#[test]
fn m17_a_missing_bounds_source_fails_the_run() {
    let mut app = world_app();
    for (bounds, guid) in [
        (ProcBounds::FromSpline { margin_m: 1.0 }, 43),
        (ProcBounds::FromShape, 44),
    ] {
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds,
                    ..ProcGenerator::new("one.pgraph.ron")
                },
                EntityGuid(Guid(guid)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id();
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Failed));
        let code = app
            .world()
            .get::<ProcGenState>(e)
            .and_then(|s| s.last_error.clone())
            .map(|x| x.1);
        // One code for both checks since integration (procgen-runtime-gaps, ADR-2421; ADR-2451).
        assert_eq!(code.as_deref(), Some("procgen_bounds_missing"));
    }
}

fn flat_terrain(h: f32) -> regia_terrain::Terrain {
    let map = regia_terrain::TerrainMap::from_heights(129, 129, vec![h; 129 * 129], 256.0, 100.0, -50.0)
        .expect("map");
    regia_terrain::Terrain::new(
        &map.to_heightfield(),
        256.0,
        100.0,
        -50.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain")
}

/// The sampled ground height with two terrains spawned in `order`.
fn ground_height(order: [(f32, u128); 2]) -> f32 {
    let mut app = world_app();
    for (h, guid) in order {
        app.world_mut().spawn((
            flat_terrain(h),
            EntityGuid(Guid(guid)),
            Transform::default(),
            GlobalTransform::default(),
        ));
    }
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(20.0, 20.0, 20.0),
                },
                seed: Some(3),
                ..ProcGenerator::new("ground.pgraph.ron")
            },
            EntityGuid(Guid(45)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let p = out.0.points.get("points").expect("exported points");
    assert!(!p.is_empty());
    p.position[0][1]
}

/// L13: with two terrains, the captured one does not depend on spawn (query) order.
#[test]
fn l13_two_terrains_capture_the_same_ground_in_any_spawn_order() {
    let a = ground_height([(1.0, 7), (5.0, 9)]);
    let b = ground_height([(5.0, 9), (1.0, 7)]);
    assert_eq!(a.to_bits(), b.to_bits(), "{a} vs {b}");
}

/// L7: `tiles.tier` reports the same `setting_type` code as every other setting check.
#[test]
fn l7_tier_setting_errors_use_setting_type() {
    let g = graph(
        vec![
            (1, node("gen.one", &[])),
            (2, node("tiles.tier", &[("cell_m", "Huge")])),
        ],
        &[(1, "out", 2, "in")],
    );
    let errors = g
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("bad tier");
    assert!(errors.has("setting_type"), "{:?}", errors.0);
}

/// L3: settings written by a newer build (`v` above the registry's version) are refused instead of
/// being read under the old schema.
#[test]
fn l3_newer_node_settings_are_refused() {
    let entry = NodeEntry {
        v: Some(999),
        ..NodeEntry::new("gen.one")
    };
    let g: ProcGraph = graph(vec![(1, entry)], &[]);
    let errors = g
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("newer settings");
    assert!(errors.has("setting_version_newer"), "{:?}", errors.0);
}

/// L8: mistyped list entries fail compile with `setting_type` instead of a silent default.
#[test]
fn l8_mistyped_settings_fail_compile() {
    let compile_one = |ty: &str, settings: &[(&str, &str)], pin: &str| {
        let g = graph(
            vec![(1, node("gen.one", &[])), (2, node(ty, settings))],
            &[(1, "out", 2, pin)],
        );
        g.compile(&ProcNodeRegistry::with_builtins())
    };
    for (ty, settings) in [
        ("flow.match", vec![("key", "\"item.kind\""), ("cases", "[1.5]")]),
        ("filter.range", vec![("inclusive", "[1, true]")]),
        ("attr.math", vec![("op", "add"), ("b", "(1.0, \"x\")")]),
        ("attr.set", vec![("value", "(1.0, \"x\")")]),
    ] {
        let errors = compile_one(ty, &settings, "in").expect_err(ty);
        assert!(errors.has("setting_type"), "{ty}: {:?}", errors.0);
    }
    // The well-typed forms still compile.
    assert!(
        compile_one(
            "flow.match",
            &[("key", "\"item.kind\""), ("cases", "[\"a\", 2]")],
            "in"
        )
        .is_ok()
    );
    assert!(compile_one("filter.range", &[("inclusive", "[false, true]")], "in").is_ok());
    assert!(compile_one("attr.math", &[("op", "add"), ("b", "(1.0, 2.0)")], "in").is_ok());
    assert!(compile_one("attr.set", &[("value", "\"text\"")], "in").is_ok());
}

/// L9: a table column named like a built-in (`density`) is reported, not silently dropped.
#[test]
fn l9_reserved_table_columns_are_reported() {
    let g = graph(
        vec![
            (1, node("in.table", &[("asset", "\"t.table.ron\"")])),
            (2, node("points.from_table", &[])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let mut snap = (*snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None)).clone();
    snap.tables.insert(
        "t.table.ron".to_string(),
        Table {
            rows: 2,
            columns: vec![
                ("density".to_string(), AttrColumn::Float(vec![0.25, 0.5].into())),
                ("w".to_string(), AttrColumn::Float(vec![1.0, 2.0].into())),
            ],
        },
    );
    let r = run_with(&compile(&g), &RunInputs::new(Arc::new(snap), 1)).expect("run");
    let line = r
        .report
        .nodes
        .iter()
        .find(|n| n.id == 2)
        .expect("from_table line");
    assert!(
        line.warnings
            .iter()
            .any(|w| w.0 == "column_reserved" && w.1.contains("density")),
        "{:?}",
        line.warnings
    );
}

/// L10: `points.flat_only` with no ground says so, like `sample.ground`.
#[test]
fn l10_flat_only_without_ground_warns_no_terrain() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(2.0, 1.0, 2.0)")])),
            (2, node("in.ground", &[])),
            (3, node("points.flat_only", &[])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "ground"), (3, "out", 4, "in")],
    );
    let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1);
    let r = run_with(&compile(&g), &inputs).expect("run");
    let line = r.report.nodes.iter().find(|n| n.id == 3).expect("flat_only line");
    assert!(
        line.warnings.iter().any(|w| w.0 == "no_terrain"),
        "{:?}",
        line.warnings
    );
}

/// L11: a mask rectangle must be finite and ordered; an oversized mask reports instead of
/// overflowing the size math.
#[test]
fn l11_mask_rectangles_are_validated() {
    for rect in [
        [f32::INFINITY, 0.0, 1.0, 1.0],
        [0.0, f32::NAN, 1.0, 1.0],
        [2.0, 0.0, 1.0, 1.0],
        [0.0, 3.0, 1.0, 1.0],
    ] {
        assert!(MaskData::new(rect, 1, 1, vec![0.5], false).is_err(), "{rect:?}");
    }
    assert!(MaskData::new([0.0, 0.0, 1.0, 1.0], 1, 1, vec![0.5], false).is_ok());
    assert!(MaskData::new([0.0, 0.0, 1.0, 1.0], 70_000, 70_000, Vec::new(), false).is_err());
}

/// L16: two bare mesh items sample different triangles and barycentrics; every placement used to
/// share the seed of the origin, so the two items got identical samples.
#[test]
fn l16_bare_mesh_items_sample_independently() {
    let mut snap = (*snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None)).clone();
    snap.meshes.insert(
        "quad.glb".into(),
        Arc::new(regia_procgen::data::MeshData {
            asset: "quad.glb".into(),
            positions: vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [2.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            normals: Vec::new(),
            indices: vec![0, 2, 1, 0, 3, 2],
        }),
    );
    let g = graph(
        vec![
            (1, node("in.mesh", &[("asset", "\"quad.glb\"")])),
            (2, node("in.mesh", &[("asset", "\"quad.glb\"")])),
            (3, node("items.gather", &[])),
            (4, node("sample.mesh_surface", &[("per_m2", "20.0")])),
            (5, node("graph.output", &[])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let r = run_with(&compile(&g), &RunInputs::new(Arc::new(snap), 1)).expect("run");
    let items = &r.output.graph_outputs.get("out").expect("out").0;
    assert_eq!(items.len(), 2);
    let a = items[0].as_points().expect("points").position.clone();
    let b = items[1].as_points().expect("points").position.clone();
    assert_eq!(a.len(), 40);
    assert_ne!(a, b, "both items sampled the same points");
}
