// SPDX-License-Identifier: MPL-2.0
//! `in.entities { as: splines | shapes }` (spec 63 § 3, spec 64 § 2): splines are captured in world
//! space, and editing a read spline marks the generator stale (headless app, inline evaluation,
//! fixed frame counts).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource, ProcOutput,
    request_run,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::{EntityGuid, Tags};
use regia_scene::shape::{Shape, ShapeKind};
use regia_scene::spline::{Spline, TangentMode};
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

fn graph(form: &str, then: &str) -> String {
    ProcGraph {
        nodes: [
            (
                1,
                n(
                    "in.entities",
                    &[("tags", "[\"road\"]"), ("within", "world"), ("as", form)],
                ),
            ),
            (2, n(then, &[("step_m", "5.0")])),
            (3, n("out.points", &[])),
        ]
        .into_iter()
        .collect(),
        edges: vec![Edge::new(1, "out", 2, "in"), Edge::new(2, "out", 3, "in")],
        ..ProcGraph::default()
    }
    .to_ron()
}

fn app() -> App {
    let mut files = BTreeMap::new();
    files.insert("roads.pgraph.ron".to_string(), graph("splines", "sample.spline"));
    // Shapes as a region for a grid sampler.
    files.insert(
        "shapes.pgraph.ron".to_string(),
        ProcGraph {
            nodes: [
                (
                    1,
                    n(
                        "in.entities",
                        &[("tags", "[\"road\"]"), ("within", "world"), ("as", "shapes")],
                    ),
                ),
                (2, n("sample.region", &[("spacing_m", "1.0")])),
                (3, n("out.points", &[])),
            ]
            .into_iter()
            .collect(),
            edges: vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")],
            ..ProcGraph::default()
        }
        .to_ron(),
    );
    // Roads: the profile comes from the entity's RoadOf.
    files.insert(
        "roads/two_lane.road.ron".to_string(),
        "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), samples_per_m: 1.0)".to_string(),
    );
    files.insert(
        "ribbon.pgraph.ron".to_string(),
        ProcGraph {
            nodes: [
                (
                    1,
                    n(
                        "in.entities",
                        &[("tags", "[\"road\"]"), ("within", "world"), ("as", "splines")],
                    ),
                ),
                (2, n("road.ribbon", &[])),
                (3, n("out.points", &[])),
            ]
            .into_iter()
            .collect(),
            edges: vec![
                Edge::new(1, "out", 2, "splines"),
                Edge::new(2, "out_edges", 3, "in"),
            ],
            ..ProcGraph::default()
        }
        .to_ron(),
    );
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(AppTypeRegistry::default());
    app
}

fn generator(app: &mut App, graph: &str) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::splat(200.0),
                },
                ..ProcGenerator::new(graph)
            },
            EntityGuid(Guid(9)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

#[test]
fn splines_are_captured_in_world_space_and_edits_mark_stale() {
    let mut app = app();
    let mut s = Spline::through([Vec3::ZERO, Vec3::new(20.0, 0.0, 0.0)]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let road = app
        .world_mut()
        .spawn((
            EntityGuid(Guid(70)),
            Tags::from_iter_str(["road"]),
            Transform::from_translation(Vec3::new(100.0, 2.0, 0.0)),
            s,
        ))
        .id();
    let e = generator(&mut app, "roads.pgraph.ron");
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let pts = &out.0.points["points"];
    // 20 m at 5 m steps: five points from x = 100 to 120 at y = 2.
    assert_eq!(pts.len(), 5);
    assert!((pts.position[0][0] - 100.0).abs() < 1e-4 && (pts.position[0][1] - 2.0).abs() < 1e-4);
    assert!((pts.position[4][0] - 120.0).abs() < 1e-4);
    app.update();
    let stale = |app: &App| app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale);
    assert!(!stale(&app));
    app.world_mut().get_mut::<Spline>(road).expect("spline").points[1]
        .position
        .z = 5.0;
    app.update();
    assert!(stale(&app), "editing the read spline marks the generator stale");
}

#[test]
fn shapes_are_regions() {
    let mut app = app();
    app.world_mut().spawn((
        EntityGuid(Guid(71)),
        Tags::from_iter_str(["road"]),
        Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
        Shape::new(ShapeKind::Box {
            half_extents: Vec3::new(2.0, 1.0, 2.0),
        }),
    ));
    let e = generator(&mut app, "shapes.pgraph.ron");
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let pts = &out.0.points["points"];
    // A 4 m × 4 m box sampled every 1 m.
    assert!((16..=25).contains(&pts.len()), "{}", pts.len());
    assert!(
        pts.position
            .iter()
            .all(|p| (8.0..=12.0).contains(&p[0]) && (-2.0..=2.0).contains(&p[2]))
    );
}

#[test]
fn road_of_profiles_reach_the_road_nodes() {
    let mut app = app();
    let mut s = Spline::through([Vec3::ZERO, Vec3::new(30.0, 0.0, 0.0)]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    app.world_mut().spawn((
        EntityGuid(Guid(72)),
        Tags::from_iter_str(["road"]),
        Transform::default(),
        s,
        regia_procgen::road::RoadOf::new("roads/two_lane.road.ron"),
    ));
    let e = generator(&mut app, "ribbon.pgraph.ron");
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let edges = &out.0.points["points"];
    // 31 rows (30 m at 1 per m, both ends), two edge points each, 3.5 m either side.
    assert_eq!(edges.len(), 62);
    assert!(edges.position.iter().all(|p| (p[2].abs() - 3.5).abs() < 1e-4));
    assert_eq!(out.0.meshes.len(), 1);
}

/// `in.self` adds the entity's own `Shape` and `Spline` (a closed one also as a polygon), tagged
/// `self`, after the bounds; `in.entities` scopes an exclusion spline with its width.
#[test]
fn in_self_lists_the_generators_own_spline_and_shape_and_exclusions_carry_a_width() {
    use regia_procgen::Payload;
    let g = ProcGraph {
        nodes: [
            (1, n("in.self", &[])),
            (
                2,
                n(
                    "in.entities",
                    &[
                        ("tags", "[\"procgen_exclude\"]"),
                        ("as", "\"splines\""),
                        ("within", "\"world\""),
                    ],
                ),
            ),
            (3, n("graph.output", &[("pin", "\"a\"")])),
            (4, n("graph.output", &[("pin", "\"b\"")])),
        ]
        .into_iter()
        .collect(),
        edges: vec![Edge::new(1, "out", 3, "in"), Edge::new(2, "out", 4, "in")],
        ..ProcGraph::default()
    };
    let files: BTreeMap<String, String> = [("s.pgraph.ron".to_string(), g.to_ron())].into();
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        keep_debug: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let mut square = Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 10.0),
        Vec3::new(0.0, 0.0, 10.0),
    ])
    .closed();
    square
        .points
        .iter_mut()
        .for_each(|p| p.mode = TangentMode::Linear);
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator::new("s.pgraph.ron"),
            square,
            Shape::default(),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let mut river = Spline::through([Vec3::new(-5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.0)]);
    river.points[1].scale.x = 12.0;
    app.world_mut().spawn((
        river,
        Tags::from_iter_str(["procgen_exclude"]),
        EntityGuid(Guid(2)),
        Transform::default(),
        GlobalTransform::default(),
    ));
    assert_eq!(
        regia_procgen::world::run_now(app.world_mut(), e),
        Some(regia_procgen::world::ProcGenRunState::Ready)
    );
    let st = app
        .world()
        .get::<regia_procgen::world::ProcGenState>(e)
        .expect("state");
    let debug = st.debug.clone().expect("debug outputs");
    let items = &debug[&1][0].0;
    let kinds: Vec<&str> = items
        .iter()
        .map(|i| match &i.payload {
            Payload::Shape(_) => "shape",
            Payload::Spline(_) => "spline",
            Payload::Polygon(_) => "polygon",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["shape", "shape", "polygon", "spline"]);
    assert!(items.iter().all(|i| i.tags.contains("self")));
    let exclusions = &debug[&2][0].0;
    assert_eq!(
        exclusions[0].scope.get("exclude_width_m"),
        Some(&regia_procgen::AttrValue::Float(12.0))
    );
}
