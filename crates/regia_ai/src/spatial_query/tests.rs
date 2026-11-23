// SPDX-License-Identifier: MPL-2.0
//! Spatial-query acceptance tests (`docs/specs/33-spatial-queries.md` § 7). No golden, GPU or
//! wall clock: every check counts units or compares exact values.

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_nav::NavMeshBuilder;
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::components::{Name, Tags};
use regia_scene::{GlobalTransform, Transform};

use super::*;
use crate::blackboard::{BbValue, Blackboard};
use crate::task::{BtContext, BtStatus, BtTask};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SpatialQueryPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn physics_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, SpatialQueryPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn spawn_at(app: &mut App, pos: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(pos),
            GlobalTransform(Affine3A::from_translation(pos)),
        ))
        .id()
}

fn spawn_tagged(app: &mut App, pos: Vec3, tag: &str) -> Entity {
    let e = spawn_at(app, pos);
    app.world_mut().entity_mut(e).insert(Tags::from_iter_str([tag]));
    e
}

#[cfg(feature = "spatial-query-probe")]
fn move_to(app: &mut App, e: Entity, pos: Vec3) {
    let mut entity = app.world_mut().entity_mut(e);
    entity.insert((
        Transform::from_translation(pos),
        GlobalTransform(Affine3A::from_translation(pos)),
    ));
}

fn text(item_type: &str, generator: &str, tests: &str, extra: &str) -> String {
    format!(
        "(format_version: 1, name: \"q\", item_type: {item_type}, {extra} generator: {generator}, tests: [{tests}])"
    )
}

fn register(app: &mut App, ron: &str) -> String {
    app.world_mut()
        .resource_mut::<SpatialQueryManager>()
        .register_template_text(ron)
        .unwrap_or_else(|e| panic!("template loads: {e}"))
}

fn run(app: &mut App, req: SpatialQueryRequest) -> SpatialQueryResult {
    app.world_mut()
        .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.run_instant(world, req))
}

fn all(owner: Entity) -> SpatialQueryRequest {
    SpatialQueryRequest::new("q", owner).with_pick(Pick::All)
}

fn positions(result: &SpatialQueryResult) -> Vec<Vec3> {
    result
        .items
        .iter()
        .filter(|i| i.passed)
        .map(|i| i.position)
        .collect()
}

fn close(a: Vec3, b: Vec3) -> bool {
    (a - b).abs().max_element() <= 1e-4
}

fn assert_points(got: &[Vec3], want: &[Vec3]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!(close(*g, *w), "got {got:?}, want {want:?}");
    }
}

/// A provider yielding fixed points (exercises `Provider` anchors).
struct Points(Vec<Vec3>);
impl AnchorProvider for Points {
    fn provide(&self, _world: &World, _asker: Entity) -> AnchorData {
        AnchorData {
            points: self.0.clone(),
            entities: Vec::new(),
        }
    }
}

fn provide(app: &mut App, name: &str, points: Vec<Vec3>) {
    app.world_mut()
        .resource_mut::<SpatialQueryManager>()
        .register_provider(name, Box::new(Points(points)));
}

fn square(x0: f32, x1: f32, z0: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

fn insert_mesh(app: &mut App, squares: &[[[Vec3; 3]; 2]]) {
    let triangles: Vec<[Vec3; 3]> = squares.iter().flatten().copied().collect();
    let mesh = NavMeshBuilder::default()
        .build(&triangles)
        .expect("the test mesh builds");
    app.insert_resource(mesh);
}

fn spawn_wall(app: &mut App, center: Vec3, half: Vec3) {
    app.world_mut().spawn((
        Transform::from_translation(center),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box { half_extents: half },
            ..Collider::default()
        },
    ));
}

fn settle(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

const FLANK: &str = r#"(
    format_version: 1,
    name: "flank_position",
    item_type: Point,
    server_only: false,
    default_pick: Best,
    params: { "radius": Float(4.0) },
    generator: Lattice(center: Asker, half_size: ParamF32("radius"), spacing: 1.0),
    tests: [
        Cast(id: "sees_player", purpose: Filter, from: Candidate(height: 1.0), to: Entity(tag: "player", height: 1.0),
             shape: Line, layers: [], want_hit: false),
        Distance(id: "near", purpose: Score, to: Entity(tag: "player"), mode: Planar,
                 score: (shape: Linear, weight: -1.0, clamp_min: Seen, clamp_max: Seen)),
    ],
)"#;

// § 7 item 1 (and 5, 6): generate → filter → score → pick.
#[test]
fn the_pipeline_picks_the_best_survivor_behind_no_wall() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_tagged(&mut app, Vec3::new(10.0, 0.0, 0.0), "player");
    // A wall at x = 5 spanning z -1..1, y 0..4, between the lattice and the player.
    spawn_wall(&mut app, Vec3::new(5.0, 2.0, 0.0), Vec3::new(0.2, 2.0, 1.0));
    settle(&mut app);
    register(&mut app, FLANK);
    let result = run(&mut app, SpatialQueryRequest::new("flank_position", asker));
    assert_eq!(result.status, QueryStatus::Success, "{:?}", result.message);
    // (4, 0, 0), (4, 0, ±1) are behind the wall; (4, 0, ±2) tie, and the lower generation index
    // (row z = -2 comes first) wins.
    assert_eq!(result.items.len(), 1);
    assert!(
        close(result.items[0].position, Vec3::new(4.0, 0.0, -2.0)),
        "{:?}",
        result.items[0]
    );
    assert_eq!(result.tests[0].id, "sees_player");
    assert_eq!(result.tests[0].input, 81);
    assert!(result.tests[0].output < 81);
}

// § 7 item 5: `Cast` with want_hit false keeps exactly the points with a clear line.
#[test]
fn cast_keeps_exactly_the_points_with_a_clear_line() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_wall(&mut app, Vec3::new(3.0, 1.0, 0.0), Vec3::new(0.2, 1.0, 1.5));
    settle(&mut app);
    register(
        &mut app,
        &text(
            "Point",
            "Ring(center: Asker, radius: 6.0, count: 16, height: 0.5)",
            r#"Cast(id: "clear", purpose: Filter, from: Candidate(), to: Offset(anchor: Asker, height: 0.5), want_hit: false)"#,
            "",
        ),
    );
    let mut req = all(asker);
    req.keep_failed = true;
    let result = run(&mut app, req);
    assert_eq!(result.items.len(), 16);
    let world = app.world();
    let queries = regia_physics::WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: None,
    };
    let target = Vec3::new(0.0, 0.5, 0.0);
    let mut kept = 0;
    for item in &result.items {
        let delta = target - item.position;
        let blocked = queries
            .raycast(
                item.position,
                delta,
                delta.length(),
                regia_physics::QueryFilter::new(),
            )
            .is_some();
        assert_eq!(item.passed, !blocked, "{item:?}");
        kept += usize::from(item.passed);
    }
    assert!(kept > 0 && kept < 16, "the wall blocks some, not all: {kept}");
}

// § 7 item 2: the authoring order of filters and scorers does not change the result.
#[test]
fn authoring_order_does_not_change_the_result() {
    let tests = [
        r#"Distance(id: "inside", purpose: Filter, to: Asker, max: 5.0)"#,
        r#"Distance(id: "not_at_feet", purpose: Filter, to: Asker, min: 1.5)"#,
        r#"Distance(id: "toward_a", purpose: Score, to: Point((3.0, 0.0, 1.0)), score: (weight: -2.0))"#,
        r#"Facing(id: "ahead", purpose: Both, a: Forward(Asker), b: Between(Asker, Candidate()), min: -0.5, score: (weight: 0.5))"#,
    ];
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let mut reference: Option<Vec<(Vec3, f32)>> = None;
    let mut order = [0usize, 1, 2, 3];
    // Every permutation (Heap's algorithm, iterative).
    let mut c = [0usize; 4];
    let mut check = |order: &[usize; 4], app: &mut App| {
        let joined: Vec<&str> = order.iter().map(|i| tests[*i]).collect();
        register(
            app,
            &text(
                "Point",
                "Lattice(center: Asker, half_size: 5.0, spacing: 1.0)",
                &joined.join(", "),
                "",
            ),
        );
        let result = run(app, all(asker));
        let got: Vec<(Vec3, f32)> = result.items.iter().map(|i| (i.position, i.score)).collect();
        assert!(!got.is_empty());
        match &reference {
            None => reference = Some(got),
            Some(want) => assert_eq!(&got, want, "order {order:?}"),
        }
    };
    check(&order, &mut app);
    let mut i = 0;
    while i < 4 {
        if c[i] < i {
            if i % 2 == 0 {
                order.swap(0, i);
            } else {
                order.swap(c[i], i);
            }
            check(&order, &mut app);
            c[i] += 1;
            i = 0;
        } else {
            c[i] = 0;
            i += 1;
        }
    }
}

// § 7 item 3: `Union` runs every child and checks item types at load.
#[test]
fn union_runs_every_child_and_dedupes() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Point",
            "Union(children: [Ring(center: Asker, radius: 2.0, count: 4), AtAnchor(anchor: Point((5.0, 0.0, 0.0))), AtAnchor(anchor: Point((2.0, 0.0, 0.0)))])",
            "",
            "",
        ),
    );
    let result = run(&mut app, all(asker));
    // 4 ring points + 1 anchor point; the second anchor duplicates the ring's first point.
    assert_eq!(result.item_count, 5, "{:?}", positions(&result));
}

/// Two identical rings share a point. The dedupe epsilon equals the quantization bucket, so the
/// first occupant of a bucket matches and the scan does not walk the whole neighborhood.
#[test]
fn identical_rings_dedupe_with_linear_compares() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let _ = super::exec::take_dedupe_compares();
    register(
        &mut app,
        &text(
            "Point",
            "Union(children: [Ring(center: Asker, radius: 8.0, count: 64), Ring(center: Asker, radius: 8.0, count: 64)])",
            "",
            "",
        ),
    );
    let result = run(&mut app, all(asker));
    let compares = super::exec::take_dedupe_compares();
    assert_eq!(result.item_count, 64, "{:?}", positions(&result));
    assert!(
        compares <= result.item_count,
        "dedupe compared {compares} positions for {} items",
        result.item_count
    );
}

#[test]
fn union_rejects_an_entity_child_in_a_point_query_unless_mixed() {
    let bad = text(
        "Point",
        "Union(children: [Ring(radius: 2.0, count: 4), NearbyEntities(tag: Some(\"crate\"))])",
        "",
        "",
    );
    let error = SpatialQueryAsset::parse(&bad).expect_err("mixed types");
    assert_eq!(error.code(), "sq_item_type_mismatch");

    let mixed = text(
        "Point",
        "Union(children: [Ring(radius: 2.0, count: 4), NearbyEntities(tag: Some(\"crate\"))], allow_mixed: true)",
        "",
        "",
    );
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_tagged(&mut app, Vec3::new(9.0, 0.0, 0.0), "crate");
    register(&mut app, &mixed);
    let result = run(&mut app, all(asker));
    assert_eq!(result.item_count, 5);
    assert!(
        result.items.iter().all(|i| i.entity.is_none()),
        "entities flatten to positions"
    );

    let wrong_top = text("Entity", "Ring(radius: 2.0, count: 4)", "", "");
    assert_eq!(
        SpatialQueryAsset::parse(&wrong_top)
            .expect_err("point generator, entity query")
            .code(),
        "sq_item_type_mismatch"
    );
}

// § 7 item 4: `NearbyEntities` radius rules.
#[test]
fn nearby_entities_radius_is_inclusive_and_non_positive_finds_nothing() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let a = spawn_tagged(&mut app, Vec3::new(1.0, 0.0, 0.0), "crate");
    let b = spawn_tagged(&mut app, Vec3::new(0.0, 0.0, 2.0), "crate");
    let c = spawn_tagged(&mut app, Vec3::new(-3.0, 0.0, 0.0), "crate");
    spawn_tagged(&mut app, Vec3::new(0.5, 0.0, 0.0), "barrel");
    let count = |app: &mut App, radius: &str| {
        register(
            app,
            &text(
                "Entity",
                &format!("NearbyEntities(tag: Some(\"crate\"), radius: {radius})"),
                "",
                "",
            ),
        );
        run(app, all(asker))
    };
    assert_eq!(count(&mut app, "Some(0.0)").item_count, 0);
    assert_eq!(count(&mut app, "Some(-1.0)").item_count, 0);
    let everything = count(&mut app, "None");
    assert_eq!(everything.item_count, 3);
    let mut found: Vec<Entity> = everything.items.iter().filter_map(|i| i.entity).collect();
    found.sort_by_key(|e| e.index_u32());
    let mut want = vec![a, b, c];
    want.sort_by_key(|e| e.index_u32());
    assert_eq!(found, want);
    assert_eq!(
        count(&mut app, "Some(2.0)").item_count,
        2,
        "radius 2 includes the entity at 2"
    );
}

// § 7 item 6: `Distance` with weight -1 ranks nearer survivors higher.
#[test]
fn negative_distance_weight_ranks_nearer_items_higher() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    provide(
        &mut app,
        "pts",
        vec![
            Vec3::new(5.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(3.0, 0.0, 0.0),
        ],
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"Distance(id: "near", purpose: Score, to: Asker, score: (weight: -1.0))"#,
            "",
        ),
    );
    let result = run(&mut app, all(asker));
    assert_points(
        &positions(&result),
        &[
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::new(5.0, 0.0, 0.0),
        ],
    );
    // Linear over Seen bounds 1..5: contributions -0, -0.5, -1; normalized by |weight| 1.
    let scores: Vec<f32> = result.items.iter().map(|i| i.score).collect();
    assert!(
        (scores[0] - 0.0).abs() < 1e-6 && (scores[1] + 0.5).abs() < 1e-6 && (scores[2] + 1.0).abs() < 1e-6
    );
}

// § 7 item 7: exact point sets.
#[test]
fn ring_points_for_count_gap_and_arc() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(&mut app, &text("Point", "Ring(radius: 2.0, count: 4)", "", ""));
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 2.0),
            Vec3::new(-2.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -2.0),
        ],
    );
    // A gap wider than the circumference: one point.
    register(&mut app, &text("Point", "Ring(radius: 1.0, gap: 7.0)", "", ""));
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[Vec3::new(1.0, 0.0, 0.0)],
    );
    // A 90 degree arc toward +Z with three points: 45, 90, 135 degrees.
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 3, height: 1.0, arc: Some((toward: Between(Asker, Point((0.0, 0.0, 5.0))), degrees: 90.0)))",
            "",
            "",
        ),
    );
    let s = 2.0f32.sqrt();
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[
            Vec3::new(s, 1.0, s),
            Vec3::new(0.0, 1.0, 2.0),
            Vec3::new(-s, 1.0, s),
        ],
    );
    // radius <= 0 or count 0: nothing.
    register(&mut app, &text("Point", "Ring(radius: 0.0, count: 4)", "", ""));
    assert_eq!(run(&mut app, all(asker)).item_count, 0);
    register(&mut app, &text("Point", "Ring(radius: 2.0, count: 0)", "", ""));
    assert_eq!(run(&mut app, all(asker)).item_count, 0);
}

#[test]
fn wedge_points_with_the_center() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // Facing the asker's forward (-Z, angle -90 degrees); rays at -135, -90, -45 degrees.
    register(
        &mut app,
        &text(
            "Point",
            "Wedge(facing: Forward(Asker), degrees: 90.0, range: 2.0, angle_step: 45.0, aligned_spacing: 1.0, include_center: true)",
            "",
            "",
        ),
    );
    let h = 0.5f32.sqrt();
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[
            Vec3::ZERO,
            Vec3::new(-h, 0.0, -h),
            Vec3::new(-2.0 * h, 0.0, -2.0 * h),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, 0.0, -2.0),
            Vec3::new(h, 0.0, -h),
            Vec3::new(2.0 * h, 0.0, -2.0 * h),
        ],
    );
    // angle_step below 1 degree clamps to 1: a 2 degree wedge has three rays.
    register(
        &mut app,
        &text(
            "Point",
            "Wedge(facing: Forward(Asker), degrees: 2.0, range: 1.0, angle_step: 0.1, aligned_spacing: 1.0)",
            "",
            "",
        ),
    );
    assert_eq!(run(&mut app, all(asker)).item_count, 3);
}

#[test]
fn annulus_spiral_rotates_each_ring() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // outer < inner is swapped.
    register(
        &mut app,
        &text(
            "Point",
            "Annulus(inner_radius: 3.0, outer_radius: 1.0, rings: 2, points_per_ring: 2, spiral: Some(90.0))",
            "",
            "",
        ),
    );
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 3.0),
            Vec3::new(0.0, 0.0, -3.0),
        ],
    );
    // One ring uses the mid radius.
    register(
        &mut app,
        &text(
            "Point",
            "Annulus(inner_radius: 1.0, outer_radius: 3.0, rings: 1, points_per_ring: 1)",
            "",
            "",
        ),
    );
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[Vec3::new(2.0, 0.0, 0.0)],
    );
}

#[test]
fn lattice_is_row_major_and_projects_onto_the_navmesh() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::new(0.0, 0.5, 0.0));
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 1.0, spacing: 1.0)", "", ""),
    );
    let got = positions(&run(&mut app, all(asker)));
    assert_eq!(got.len(), 9);
    // X varies fastest, then Z: index 0 is the -X/-Z corner, index 1 steps +X.
    let y = 0.5;
    let expect = [
        Vec3::new(-1.0, y, -1.0),
        Vec3::new(0.0, y, -1.0),
        Vec3::new(1.0, y, -1.0),
        Vec3::new(-1.0, y, 0.0),
        Vec3::new(0.0, y, 0.0),
        Vec3::new(1.0, y, 0.0),
        Vec3::new(-1.0, y, 1.0),
        Vec3::new(0.0, y, 1.0),
        Vec3::new(1.0, y, 1.0),
    ];
    for (i, want) in expect.iter().enumerate() {
        assert!(close(got[i], *want), "index {i}: got {:?} want {want:?}", got[i]);
    }

    insert_mesh(&mut app, &[square(-4.0, 4.0, -4.0, 4.0)]);
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 6.0, spacing: 2.0, project_to_navmesh: true)",
            "",
            "",
        ),
    );
    let got = positions(&run(&mut app, all(asker)));
    // 7 x 7 points; the ring at |x| or |z| = 6 has no mesh within the extents.
    assert_eq!(got.len(), 25, "{got:?}");
    assert!(
        got.iter()
            .all(|p| p.y.abs() < 0.3 && p.x.abs() <= 4.0 && p.z.abs() <= 4.0)
    );
}

#[test]
fn reach_lattice_drops_an_island() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    insert_mesh(
        &mut app,
        &[square(-4.0, 4.0, -4.0, 4.0), square(9.0, 15.0, -3.0, 3.0)],
    );
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 12.0, spacing: 2.0, project_to_navmesh: true)",
            "",
            "",
        ),
    );
    let projected = positions(&run(&mut app, all(asker)));
    assert!(projected.iter().any(|p| p.x > 8.0), "the island is on the mesh");
    register(
        &mut app,
        &text(
            "Point",
            "ReachLattice(half_size: 12.0, spacing: 2.0, reach_limit: 10.0)",
            "",
            "",
        ),
    );
    let reachable = positions(&run(&mut app, all(asker)));
    assert!(!reachable.is_empty());
    assert!(
        reachable.iter().all(|p| p.x < 8.0),
        "the island is unreachable: {reachable:?}"
    );
}

#[test]
fn reach_lattice_without_a_navmesh_fails_with_sq_no_navmesh() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text("Point", "ReachLattice(half_size: 2.0, spacing: 1.0)", "", ""),
    );
    let result = run(&mut app, all(asker));
    assert_eq!(result.status, QueryStatus::Failed);
    assert_eq!(result.code, Some("sq_no_navmesh"));
}

#[test]
fn a_huge_lattice_fails_with_sq_too_many_items() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 200.0, spacing: 1.0)", "", ""),
    );
    assert_eq!(run(&mut app, all(asker)).code, Some("sq_too_many_items"));
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 2.0, spacing: 0.0)", "", ""),
    );
    assert_eq!(run(&mut app, all(asker)).code, Some("sq_too_many_items"));
}

#[test]
fn a_cast_without_physics_fails_with_sq_missing_system() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 4)",
            r#"Cast(id: "c", purpose: Filter, to: Asker, want_hit: false)"#,
            "",
        ),
    );
    let result = run(&mut app, all(asker));
    assert_eq!(result.code, Some("sq_missing_system"));
    assert!(result.message.as_deref().is_some_and(|m| m.contains("physics")));
}

// § 7 item 8: float scoring with each clamp source, end to end.
#[test]
fn clamp_sources_fixed_filter_bound_and_seen() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    provide(
        &mut app,
        "pts",
        (1..=8).map(|x| Vec3::new(x as f32, 0.0, 0.0)).collect(),
    );
    let scores = |app: &mut App, clamp: &str| -> Vec<(f32, f32)> {
        register(
            app,
            &text(
                "Point",
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Distance(id: \"d\", purpose: Both, to: Asker, min: 2.0, max: 6.0, score: ({clamp}))"
                ),
                "",
            ),
        );
        let mut result = run(app, all(asker));
        result.items.sort_by(|a, b| a.position.x.total_cmp(&b.position.x));
        result.items.iter().map(|i| (i.position.x, i.score)).collect()
    };
    let filter_bound = scores(&mut app, "clamp_min: FilterBound, clamp_max: FilterBound");
    assert_eq!(filter_bound.len(), 5, "the filter keeps 2..=6");
    for (x, s) in &filter_bound {
        assert!((s - (x - 2.0) / 4.0).abs() < 1e-6, "{x} -> {s}");
    }
    let fixed = scores(&mut app, "clamp_min: Fixed(0.0), clamp_max: Fixed(10.0)");
    for (x, s) in &fixed {
        assert!((s - x / 10.0).abs() < 1e-6, "{x} -> {s}");
    }
    let zero_origin = scores(
        &mut app,
        "clamp_min: Fixed(4.0), clamp_max: Fixed(8.0), origin: Zero",
    );
    for (x, s) in &zero_origin {
        assert!((s - x.max(4.0) / 8.0).abs() < 1e-6, "{x} -> {s}");
    }
    let peak = scores(&mut app, "peak_at: Some(4.0)");
    // Seen bounds 2..6 around a peak at 4: denominator 2.
    for (x, s) in &peak {
        assert!((s - (1.0 - (x - 4.0).abs() / 2.0)).abs() < 1e-6, "{x} -> {s}");
    }
    let quadratic = scores(&mut app, "shape: Quadratic");
    for (x, s) in &quadratic {
        let n = (x - 2.0) / 4.0;
        assert!((s - n * n).abs() < 1e-6, "{x} -> {s}");
    }
}

#[test]
fn filter_bound_clamp_without_the_bound_is_sq_no_threshold() {
    let bad = text(
        "Point",
        "Ring(radius: 2.0, count: 4)",
        r#"Distance(id: "d", purpose: Score, to: Asker, score: (clamp_min: FilterBound))"#,
        "",
    );
    let error = SpatialQueryAsset::parse(&bad).expect_err("no min");
    assert_eq!(error.code(), "sq_no_threshold");
}

// § 7 item 9: several anchor values and an empty anchor.
#[test]
fn multi_value_anchors_require_and_combine() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_tagged(&mut app, Vec3::new(0.0, 0.0, 0.0), "enemy");
    spawn_tagged(&mut app, Vec3::new(4.0, 0.0, 0.0), "enemy");
    provide(
        &mut app,
        "pts",
        vec![
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(6.0, 0.0, 0.0),
        ],
    );
    let survivors = |app: &mut App, require: &str| {
        register(
            app,
            &text(
                "Point",
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Distance(id: \"d\", purpose: Filter, to: Entity(tag: Some(\"enemy\")), max: 2.5, require: {require})"
                ),
                "",
            ),
        );
        let mut got: Vec<f32> = positions(&run(app, all(asker))).iter().map(|p| p.x).collect();
        got.sort_by(f32::total_cmp);
        got
    };
    // Distances (to 0, to 4): x=1 (1, 3), x=2 (2, 2), x=6 (6, 2).
    assert_eq!(survivors(&mut app, "Every"), vec![2.0]);
    assert_eq!(survivors(&mut app, "Some"), vec![1.0, 2.0, 6.0]);

    // Combine over shaped values with Fixed 0..4 Linear bounds: n = d / 4.
    let combined = |app: &mut App, op: &str| -> Vec<f32> {
        register(
            app,
            &text(
                "Point",
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Distance(id: \"d\", purpose: Score, to: Entity(tag: Some(\"enemy\")), combine: {op}, score: (clamp_min: Fixed(0.0), clamp_max: Fixed(4.0)))"
                ),
                "",
            ),
        );
        let mut result = run(app, all(asker));
        result.items.sort_by(|a, b| a.position.x.total_cmp(&b.position.x));
        result.items.iter().map(|i| i.score).collect()
    };
    let near =
        |a: &[f32], b: &[f32]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
    // x=1: (0.25, 0.75); x=2: (0.5, 0.5); x=6: (1.0 clamped, 0.5).
    assert!(near(&combined(&mut app, "Mean"), &[0.5, 0.5, 0.75]));
    assert!(near(&combined(&mut app, "Lowest"), &[0.25, 0.5, 0.5]));
    assert!(near(&combined(&mut app, "Highest"), &[0.75, 0.5, 1.0]));
    assert!(near(&combined(&mut app, "Product"), &[0.1875, 0.25, 0.5]));
}

#[test]
fn an_empty_anchor_fails_filters_and_adds_nothing_to_scores() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 4)",
            r#"Distance(id: "d", purpose: Filter, to: Entity(tag: Some("nobody")), max: 100.0)"#,
            "",
        ),
    );
    let mut req = all(asker);
    req.keep_failed = true;
    let result = run(&mut app, req);
    assert_eq!(result.item_count, 0);
    assert_eq!(result.items.len(), 4);
    assert!(result.items.iter().all(|i| !i.passed
        && i.failed_test.as_deref() == Some("d")
        && i.failed_reason.as_deref() == Some("no_anchor")));

    // A score test with an empty anchor contributes nothing (not zero): only `near` counts, so the
    // normalized score is `near`'s alone.
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 4)",
            r#"Distance(id: "d", purpose: Score, to: Entity(tag: Some("nobody")), score: (weight: 5.0)),
               Facing(id: "near", purpose: Score, a: Forward(Asker), b: Between(Asker, Candidate()), score: (clamp_min: Fixed(-1.0), clamp_max: Fixed(1.0)))"#,
            "",
        ),
    );
    let result = run(&mut app, all(asker));
    let best = result.best().expect("a winner");
    assert_eq!(best.contributions.len(), 1);
    assert_eq!(best.contributions[0].0, "near");
    assert!(
        (best.score - 1.0).abs() < 1e-6,
        "the point straight ahead (-Z) scores 1: {best:?}"
    );
    assert!(close(best.position, Vec3::new(0.0, 0.0, -2.0)));
}

// § 7 item 10: picks.
#[test]
fn sample_picks_inside_the_band_reproducibly_and_covers_it() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    provide(
        &mut app,
        "pts",
        (0..10).map(|x| Vec3::new(x as f32, 0.0, 0.0)).collect(),
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"Distance(id: "far", purpose: Score, to: Asker)"#,
            "",
        ),
    );
    // Scores are x / 9 (Seen bounds 0..9): band 0.1 keeps x >= 8.1 (only 9), band 0.25 keeps
    // x >= 6.75 (7, 8, 9).
    for (band, members) in [(0.1f32, vec![9.0f32]), (0.25, vec![7.0, 8.0, 9.0])] {
        let pick = |app: &mut App, seed: u64| {
            let req = SpatialQueryRequest::new("q", asker)
                .with_pick(Pick::Sample { band })
                .with_seed(seed);
            run(app, req).best().map(|i| i.position.x)
        };
        assert_eq!(pick(&mut app, 7), pick(&mut app, 7), "same seed, same pick");
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..200u64 {
            let x = pick(&mut app, seed).expect("a pick");
            assert!(members.contains(&x), "band {band}: {x} is outside {members:?}");
            seen.insert(x as i64);
        }
        assert_eq!(
            seen.len(),
            members.len(),
            "band {band}: every member is picked eventually"
        );
    }
}

#[test]
fn all_orders_by_score_then_generation_index_and_best_takes_the_first() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 4)",
            r#"Distance(id: "planar", purpose: Score, to: Point((0.0, 0.0, 0.0)), mode: Planar)"#,
            "",
        ),
    );
    // Every point is 2 from the origin: all scores tie (lo == hi, n = 0); generation order holds.
    let result = run(&mut app, all(asker));
    let order: Vec<usize> = result.items.iter().map(|i| i.generation_index).collect();
    assert_eq!(order, vec![0, 1, 2, 3]);
    let best = run(&mut app, SpatialQueryRequest::new("q", asker));
    assert_eq!(best.items.len(), 1);
    assert_eq!(best.items[0].generation_index, 0);
}

#[test]
fn empty_survivors_fail_best_and_succeed_all() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: 2.0, count: 4)",
            r#"Distance(id: "d", purpose: Filter, to: Asker, max: 1.0)"#,
            "",
        ),
    );
    assert_eq!(
        run(&mut app, SpatialQueryRequest::new("q", asker)).status,
        QueryStatus::Failed
    );
    let all_result = run(&mut app, all(asker));
    assert_eq!(all_result.status, QueryStatus::Success);
    assert!(all_result.items.is_empty());
}

// § 7 item 11: parameters.
#[test]
fn a_missing_parameter_is_missing_param_and_an_override_applies() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text("Point", "Ring(radius: ParamF32(\"r\"), count: 4)", "", ""),
    );
    let result = run(&mut app, all(asker));
    assert_eq!(result.status, QueryStatus::MissingParam);
    assert!(result.message.as_deref().is_some_and(|m| m.contains("`r`")));
    let result = run(&mut app, all(asker).with_param("r", ParamValue::Float(3.0)));
    assert_eq!(result.status, QueryStatus::Success);
    assert!(close(positions(&result)[0], Vec3::new(3.0, 0.0, 0.0)));

    // A declared default, overridden by the request.
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: ParamF32(\"r\"), count: 4)",
            "",
            "params: { \"r\": Float(1.0) },",
        ),
    );
    assert!(close(
        positions(&run(&mut app, all(asker)))[0],
        Vec3::new(1.0, 0.0, 0.0)
    ));
    let result = run(&mut app, all(asker).with_param("r", ParamValue::Int(5)));
    assert!(close(positions(&result)[0], Vec3::new(5.0, 0.0, 0.0)));
    let result = run(&mut app, all(asker).with_param("r", ParamValue::Bool(true)));
    assert_eq!(result.code, Some("sq_bad_param"));
}

fn manager(app: &mut App) -> Mut<'_, SpatialQueryManager> {
    app.world_mut().resource_mut::<SpatialQueryManager>()
}

fn tick(app: &mut App) {
    tick_spatial_queries(app.world_mut());
}

// § 7 item 12: abort, abort_owner, fail_all, owner_lost.
#[test]
fn abort_abort_owner_fail_all_and_owner_lost() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let other = spawn_at(&mut app, Vec3::ONE);
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 20.0, spacing: 1.0)", "", ""),
    );
    manager(&mut app).config.work_units_per_tick = 10;

    let a = manager(&mut app).submit(all(asker));
    tick(&mut app);
    assert!(manager(&mut app).is_active(a));
    assert_eq!(manager(&mut app).poll(a), None, "processing");
    assert!(manager(&mut app).abort(a));
    assert!(!manager(&mut app).abort(a), "already finished");
    assert_eq!(
        manager(&mut app).poll(a).map(|r| r.status),
        Some(QueryStatus::Aborted)
    );
    assert!(!manager(&mut app).abort(RequestId(999)), "unknown");

    let b1 = manager(&mut app).submit(all(asker));
    let b2 = manager(&mut app).submit(all(asker));
    let c = manager(&mut app).submit(all(other));
    assert_eq!(manager(&mut app).abort_owner(asker), 2);
    assert_eq!(
        manager(&mut app).poll(b1).map(|r| r.status),
        Some(QueryStatus::Aborted)
    );
    assert_eq!(
        manager(&mut app).poll(b2).map(|r| r.status),
        Some(QueryStatus::Aborted)
    );
    assert!(manager(&mut app).is_active(c));

    manager(&mut app).fail_all();
    assert_eq!(
        manager(&mut app).poll(c).map(|r| r.status),
        Some(QueryStatus::Failed)
    );

    let d = manager(&mut app).submit(all(other));
    tick(&mut app);
    app.world_mut().despawn(other);
    tick(&mut app);
    assert_eq!(
        manager(&mut app).poll(d).map(|r| r.status),
        Some(QueryStatus::OwnerLost)
    );

    let stats = manager(&mut app).stats();
    assert_eq!(stats.aborted, 3);
    assert_eq!(stats.failed, 2);
    assert_eq!(stats.active, 0);
}

// § 7 item 13: async slicing.
#[test]
fn async_slicing_stays_in_budget_and_matches_run_instant() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_tagged(&mut app, Vec3::new(3.0, 0.0, 1.0), "enemy");
    spawn_tagged(&mut app, Vec3::new(-2.0, 0.0, 2.0), "enemy");
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 3.0, spacing: 1.0)",
            r#"Distance(id: "near", purpose: Both, to: Entity(tag: Some("enemy")), max: 4.0, score: (weight: -1.0)),
               Facing(id: "ahead", purpose: Score, a: Forward(Asker), b: Between(Asker, Candidate()))"#,
            "",
        ),
    );
    let reqs: Vec<SpatialQueryRequest> = (0..3).map(|i| all(asker).with_seed(i)).collect();
    let instant: Vec<SpatialQueryResult> = reqs.iter().map(|r| run(&mut app, r.clone())).collect();

    let budget = 7;
    manager(&mut app).config.work_units_per_tick = budget;
    let ids: Vec<RequestId> = reqs.iter().map(|r| manager(&mut app).submit(r.clone())).collect();
    let mut finished: Vec<(RequestId, SpatialQueryResult)> = Vec::new();
    let mut ticks = 0;
    while finished.len() < ids.len() {
        tick(&mut app);
        ticks += 1;
        assert!(ticks < 10_000, "the requests finish");
        let spent = manager(&mut app).stats().units_last_tick;
        assert!(spent <= budget, "tick {ticks} spent {spent} > {budget}");
        for id in &ids {
            if let Some(result) = manager(&mut app).poll(*id) {
                finished.push((*id, result));
            }
        }
    }
    let order: Vec<RequestId> = finished.iter().map(|(id, _)| *id).collect();
    assert_eq!(order, ids, "equal requests finish in submit order");
    for ((_, sliced), instant) in finished.iter().zip(&instant) {
        assert_eq!(sliced, instant, "sliced and instant results are identical");
    }
}

// § 3.6 work units (finding 10): the `RouteAll` default `scan_limit` pre-pass resolves a
// candidate-relative `to` once per item. Each of those resolutions is a charged unit, so a tick
// never does more of them than it was charged for, and the sliced result still equals the instant
// one.
#[test]
fn route_all_scan_limit_pre_pass_is_charged_to_the_tick_budget() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    insert_mesh(&mut app, &[square(-4.0, 4.0, -4.0, 4.0)]);
    // 25 generated items, a 25-item pre-pass, then 25 (item, anchor value) evaluations.
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 2.0, spacing: 1.0)",
            r#"RouteAll(id: "ra", purpose: Score, to: Candidate(height: 0.5), measure: Length, score: (weight: -1.0))"#,
            "",
        ),
    );
    let _ = super::exec::take_scan_resolves();
    let instant = run(&mut app, all(asker));
    assert_eq!(super::exec::take_scan_resolves(), 25, "one resolution per item");

    let budget = 5;
    manager(&mut app).config.work_units_per_tick = budget;
    let id = manager(&mut app).submit(all(asker));
    let mut ticks = 0;
    let sliced = loop {
        tick(&mut app);
        ticks += 1;
        assert!(ticks < 1_000, "the request finishes");
        let spent = manager(&mut app).stats().units_last_tick;
        let resolves = super::exec::take_scan_resolves() as u64;
        assert!(spent <= budget, "tick {ticks} spent {spent} > {budget}");
        assert!(
            resolves <= spent,
            "tick {ticks}: {resolves} pre-pass resolutions but only {spent} units charged"
        );
        if let Some(result) = manager(&mut app).poll(id) {
            break result;
        }
    };
    assert_eq!(sliced, instant, "sliced and instant results are identical");
    assert_eq!(instant.units, 75, "generate + pre-pass + evaluations");
}

#[test]
fn breadth_first_advances_the_next_request_in_the_same_tick() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // 25 generated items, then one 25-unit test.
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 2.0, spacing: 1.0)",
            r#"Distance(id: "d", purpose: Score, to: Asker)"#,
            "",
        ),
    );
    for (breadth_first, second_moves) in [(true, true), (false, false)] {
        manager(&mut app).config.work_units_per_tick = 40;
        manager(&mut app).config.breadth_first = breadth_first;
        let first = manager(&mut app).submit(all(asker));
        let second = manager(&mut app).submit(all(asker));
        tick(&mut app);
        let units = |app: &mut App, id| manager(app).units_spent(id).unwrap_or(0);
        assert_eq!(units(&mut app, first) + units(&mut app, second), 40);
        assert_eq!(
            units(&mut app, second) > 0,
            second_moves,
            "breadth_first {breadth_first}"
        );
        manager(&mut app).fail_all();
        let _ = manager(&mut app).poll(first);
        let _ = manager(&mut app).poll(second);
    }
}

// § 7 item 14: the behavior-tree leaf.
fn bt_tick(app: &mut App, task: &mut RunSpatialQuery, agent: Entity) -> BtStatus {
    let mut ctx = BtContext {
        world: app.world_mut(),
        entity: agent,
        dt: 1.0 / 60.0,
    };
    task.tick(&mut ctx)
}

#[test]
fn run_spatial_query_writes_a_vec3_for_points() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: ParamF32(\"r\"), count: 4)",
            "",
            "params: { \"r\": Float(1.0) },",
        ),
    );
    app.world_mut()
        .entity_mut(asker)
        .get_mut::<Blackboard>()
        .expect("blackboard")
        .insert("radius", 3.0f32);
    let mut task = RunSpatialQuery::new("q", "spot").with_param("r", BtParam::Key("radius".to_string()));
    assert_eq!(
        bt_tick(&mut app, &mut task, asker),
        BtStatus::Running,
        "first tick submits"
    );
    let mut status = BtStatus::Running;
    for _ in 0..10 {
        tick(&mut app);
        status = bt_tick(&mut app, &mut task, asker);
        if status != BtStatus::Running {
            break;
        }
    }
    assert_eq!(status, BtStatus::Success);
    let bb = app.world().get::<Blackboard>(asker).expect("blackboard");
    assert_eq!(bb.get("spot"), Some(&BbValue::Vec3(Vec3::new(3.0, 0.0, 0.0))));
}

#[test]
fn run_spatial_query_writes_an_entity_and_its_position() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    let pickup = spawn_tagged(&mut app, Vec3::new(2.0, 0.0, 0.0), "pickup");
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"pickup\"))", "", ""),
    );
    let mut task = RunSpatialQuery::new("q", "target");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Success);
    let bb = app.world().get::<Blackboard>(asker).expect("blackboard");
    assert_eq!(bb.get("target"), Some(&BbValue::Entity(pickup.to_bits())));
    assert_eq!(
        bb.get("target_position"),
        Some(&BbValue::Vec3(Vec3::new(2.0, 0.0, 0.0)))
    );
}

#[test]
fn run_spatial_query_failure_writes_nothing_and_reset_aborts() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"none\"))", "", ""),
    );
    let mut task = RunSpatialQuery::new("q", "target");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Failure);
    assert!(
        !app.world()
            .get::<Blackboard>(asker)
            .expect("bb")
            .contains("target")
    );

    register(
        &mut app,
        &text("Point", "Lattice(half_size: 30.0, spacing: 1.0)", "", ""),
    );
    manager(&mut app).config.work_units_per_tick = 5;
    let mut task = RunSpatialQuery::new("q", "spot");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    let id = task.request().expect("in flight");
    assert!(manager(&mut app).is_active(id));
    let mut ctx = BtContext {
        world: app.world_mut(),
        entity: asker,
        dt: 0.0,
    };
    task.reset(&mut ctx);
    assert!(!manager(&mut app).is_active(id));
    assert_eq!(manager(&mut app).stats().aborted, 1);
    assert!(task.request().is_none());
}

#[test]
fn a_registered_leaf_compiles_in_a_tree() {
    let mut registry = crate::BtTaskRegistry::default();
    RunSpatialQuery::new("q", "spot").register(&mut registry, "game::FindSpot");
    let def = crate::BehaviorTreeDef::leaf("find", "game::FindSpot");
    assert!(crate::BehaviorTree::compile(&def, &registry).is_ok());
}

// § 7 item 15: `server_only` round-trips.
#[test]
fn server_only_round_trips_and_the_spec_example_loads() {
    let asset =
        SpatialQueryAsset::parse(&FLANK.replace("server_only: false", "server_only: true")).expect("loads");
    assert!(asset.server_only);
    let again = SpatialQueryAsset::parse(&asset.to_ron().expect("serializes")).expect("reloads");
    assert_eq!(again, asset);
    // The spec's § 3.1 example, verbatim (optional fields take a bare value: RON implicit_some).
    let example = r#"(
        format_version: 1,
        name: "flank_position",
        item_type: Point,
        server_only: false,
        default_pick: Best,
        params: { "radius": Float(12.0), "need_cover": Bool(true) },
        generator: Lattice(center: Asker, half_size: ParamF32("radius"), spacing: 2.0, project_to_navmesh: true),
        tests: [
            Cast(id: "sees_player", purpose: Filter, from: Candidate(height: 1.6), to: Entity(tag: "player", height: 1.6),
                 shape: Line, layers: ["static"], want_hit: false),
            Distance(id: "near", purpose: Score, to: Entity(tag: "player"), mode: Planar,
                     score: (shape: Linear, weight: -1.0, clamp_min: Seen, clamp_max: Seen)),
        ],
    )"#;
    let asset = SpatialQueryAsset::parse(example).expect("the example loads");
    assert_eq!(asset.tests.len(), 2);
    assert_eq!(
        asset.used_params().into_iter().collect::<Vec<_>>(),
        vec!["radius".to_string()]
    );
}

// § 7 item 16: load errors carry line and column.
#[test]
fn load_errors_name_line_and_column() {
    let unknown_field = "(\n    format_version: 1,\n    name: \"q\",\n    item_type: Point,\n    colour: 3,\n    generator: Ring(radius: 1.0, count: 1),\n)";
    let error = SpatialQueryAsset::parse(unknown_field).expect_err("unknown field");
    assert_eq!(error.code(), "sq_unreadable");
    assert_eq!(error.position().0, 5, "{error}");

    let snap_on_entities = "(\n    format_version: 1,\n    name: \"q\",\n    item_type: Entity,\n    generator: NearbyEntities(),\n    tests: [\n        Snap(id: \"s\", purpose: Filter),\n    ],\n)";
    let error = SpatialQueryAsset::parse(snap_on_entities).expect_err("snap on entities");
    assert_eq!(error.code(), "sq_item_type_mismatch");
    assert_eq!(error.position(), (7, 18), "{error}");

    let duplicate = "(\n    format_version: 1,\n    name: \"q\",\n    item_type: Point,\n    generator: Ring(radius: 1.0, count: 1),\n    tests: [\n        Distance(id: \"d\", purpose: Score, to: Asker),\n        Distance(id: \"d\", purpose: Score, to: Asker),\n    ],\n)";
    let error = SpatialQueryAsset::parse(duplicate).expect_err("duplicate id");
    assert_eq!(error.code(), "sq_unreadable");
    assert_eq!(error.position().0, 8, "{error}");

    let threshold = "(\n    format_version: 1,\n    name: \"q\",\n    item_type: Point,\n    generator: Ring(radius: 1.0, count: 1),\n    tests: [\n        Distance(id: \"d\", purpose: Score, to: Asker, score: (clamp_max: FilterBound)),\n    ],\n)";
    let error = SpatialQueryAsset::parse(threshold).expect_err("no max");
    assert_eq!(error.code(), "sq_no_threshold");
    assert_eq!(error.position().0, 7, "{error}");

    let both = text("Point", "Ring(radius: 1.0, count: 1, gap: 1.0)", "", "");
    assert_eq!(
        SpatialQueryAsset::parse(&both).expect_err("count and gap").code(),
        "sq_unreadable"
    );
    let neither = text("Point", "Union(children: [Ring(radius: 1.0)])", "", "");
    assert_eq!(
        SpatialQueryAsset::parse(&neither).expect_err("no spacing").code(),
        "sq_unreadable"
    );

    let future = text("Point", "Ring(radius: 1.0, count: 1)", "", "")
        .replace("format_version: 1", "format_version: 3");
    let error = SpatialQueryAsset::parse(&future).expect_err("future version");
    assert!(error.to_string().contains("not supported"), "{error}");
}

// § 6: determinism.
#[test]
fn two_runs_are_bit_identical() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::new(0.3, 0.0, -0.7));
    spawn_tagged(&mut app, Vec3::new(3.0, 0.0, 1.0), "enemy");
    register(
        &mut app,
        &text(
            "Point",
            "Annulus(inner_radius: 1.0, outer_radius: 6.0, rings: 4, points_per_ring: 12, spiral: Some(7.5))",
            r#"Distance(id: "d", purpose: Score, to: Entity(tag: Some("enemy")), score: (shape: Root, weight: -0.7)),
               Facing(id: "f", purpose: Score, a: Forward(Asker), b: Between(Asker, Candidate()), score: (shape: Quadratic))"#,
            "",
        ),
    );
    let req = SpatialQueryRequest::new("q", asker)
        .with_pick(Pick::Sample { band: 0.3 })
        .with_seed(42);
    let a = run(&mut app, req.clone());
    let b = run(&mut app, req);
    assert_eq!(a, b);
    let bits = |r: &SpatialQueryResult| r.items.iter().map(|i| i.score.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&a), bits(&b));
}

#[test]
fn templates_load_from_files_and_reload_when_changed() {
    // pid + atomic counter, the `regia_test_utils::unique_temp_dir` scheme (that crate depends on
    // `regia`, too heavy a dev-dependency for this crate).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-sq-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("ring.spatialquery.ron");
    let write = |radius: f32| {
        let tmp = dir.join("ring.tmp");
        std::fs::write(
            &tmp,
            text("Point", &format!("Ring(radius: {radius:.1}, count: 1)"), "", ""),
        )
        .expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
    };
    write(1.0);
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    manager(&mut app).asset_root = dir.clone();
    let req = SpatialQueryRequest::new("ring.spatialquery.ron", asker);
    assert!(close(
        run(&mut app, req.clone()).items[0].position,
        Vec3::new(1.0, 0.0, 0.0)
    ));
    // Lookup by the asset name works once loaded.
    assert!(close(
        run(&mut app, SpatialQueryRequest::new("q", asker)).items[0].position,
        Vec3::new(1.0, 0.0, 0.0)
    ));
    // Force a different modification time so the change is seen on any file system.
    write(2.0);
    let file = std::fs::File::options().write(true).open(&path).expect("open");
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .expect("set mtime");
    drop(file);
    assert!(close(
        run(&mut app, req).items[0].position,
        Vec3::new(2.0, 0.0, 0.0)
    ));
    let missing = run(&mut app, SpatialQueryRequest::new("nope.spatialquery.ron", asker));
    assert_eq!(missing.code, Some("sq_unknown_template"));
    let _ = std::fs::remove_dir_all(&dir);
}

// Geometry tests not covered above: InsideZone, TagMatch, Clearance, Route, Snap.
#[test]
fn inside_zone_exact_and_bounds() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let zone = spawn_tagged(&mut app, Vec3::new(2.0, 0.0, 0.0), "zone");
    app.world_mut().entity_mut(zone).insert(Collider {
        shape: ColliderShape::Sphere { radius: 1.0 },
        ..Collider::default()
    });
    // (2.9, 0, 0.9) is outside the sphere but inside its bounding box.
    provide(
        &mut app,
        "pts",
        vec![
            Vec3::new(2.5, 0.0, 0.0),
            Vec3::new(2.9, 0.0, 0.9),
            Vec3::new(5.0, 0.0, 0.0),
        ],
    );
    let kept = |app: &mut App, mode: &str| {
        register(
            app,
            &text(
                "Point",
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!("InsideZone(id: \"z\", purpose: Filter, tag: \"zone\", mode: {mode})"),
                "",
            ),
        );
        positions(&run(app, all(asker))).len()
    };
    assert_eq!(kept(&mut app, "Exact"), 1);
    assert_eq!(kept(&mut app, "Bounds"), 2);
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, Med 5): the `InsideZone` volume preferred
/// the `GlobalTransform` snapshot over the local `Transform` chain, so on a later fixed step of a
/// frame — before propagation — it tested against the previous frame's zone. The zone's affine
/// now comes from the same chain walk the query poses use.
#[test]
fn inside_zone_reads_the_local_transform_chain() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let zone = spawn_tagged(&mut app, Vec3::new(2.0, 0.0, 0.0), "zone");
    app.world_mut().entity_mut(zone).insert(Collider {
        shape: ColliderShape::Sphere { radius: 1.0 },
        ..Collider::default()
    });
    provide(&mut app, "pts", vec![Vec3::new(5.0, 0.0, 0.0)]);
    let kept = |app: &mut App| {
        register(
            app,
            &text(
                "Point",
                "AtAnchor(anchor: Provider(\"pts\"))",
                r#"InsideZone(id: "z", purpose: Filter, tag: "zone", mode: Bounds)"#,
                "",
            ),
        );
        positions(&run(app, all(asker))).len()
    };
    assert_eq!(kept(&mut app), 0, "the point starts outside the zone");
    // Move the zone with its local `Transform` only; no propagation runs between the move and
    // the query, so the `GlobalTransform` snapshot still says (2, 0, 0).
    app.world_mut()
        .get_mut::<Transform>(zone)
        .expect("transform")
        .translation = Vec3::new(5.0, 0.0, 0.0);
    assert_eq!(
        kept(&mut app),
        1,
        "the zone moved with its local transform; the stale snapshot says the point is out"
    );
}

fn is_red(world: &World, entity: Entity, query: &str) -> bool {
    world.get::<Tags>(entity).is_some_and(|t| t.contains(query))
}

#[test]
fn tag_match_uses_the_host_matcher() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    spawn_tagged(&mut app, Vec3::new(1.0, 0.0, 0.0), "red");
    spawn_tagged(&mut app, Vec3::new(5.0, 0.0, 0.0), "blue");
    provide(
        &mut app,
        "pts",
        vec![Vec3::new(1.2, 0.0, 0.0), Vec3::new(4.9, 0.0, 0.0)],
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"TagMatch(id: "t", purpose: Filter, query: "red")"#,
            "",
        ),
    );
    assert_eq!(run(&mut app, all(asker)).code, Some("sq_missing_system"));
    manager(&mut app).set_tag_matcher("scene_tags", is_red);
    let got = positions(&run(&mut app, all(asker)));
    assert_points(&got, &[Vec3::new(1.2, 0.0, 0.0)]);
}

#[test]
fn clearance_drops_points_inside_geometry() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::new(0.0, 0.0, -10.0));
    spawn_wall(&mut app, Vec3::new(3.0, 1.0, 0.0), Vec3::new(0.5, 1.0, 0.5));
    settle(&mut app);
    provide(
        &mut app,
        "pts",
        vec![Vec3::new(3.0, 0.5, 0.0), Vec3::new(0.0, 0.5, 0.0)],
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"Clearance(id: "room", purpose: Filter, shape: Sphere(0.3))"#,
            "",
        ),
    );
    assert_points(
        &positions(&run(&mut app, all(asker))),
        &[Vec3::new(0.0, 0.5, 0.0)],
    );
}

#[test]
fn route_exists_and_length() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    insert_mesh(
        &mut app,
        &[square(-4.0, 4.0, -4.0, 4.0), square(9.0, 15.0, -3.0, 3.0)],
    );
    provide(
        &mut app,
        "pts",
        vec![
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(12.0, 0.0, 0.0),
        ],
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"Route(id: "reach", purpose: Filter, to: Asker)"#,
            "",
        ),
    );
    assert_eq!(
        positions(&run(&mut app, all(asker))).len(),
        2,
        "the island point has no path"
    );
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"RouteAll(id: "len", purpose: Score, to: Asker, measure: Length, drop_unreachable: true, score: (weight: -1.0))"#,
            "",
        ),
    );
    let got = positions(&run(&mut app, all(asker)));
    assert_points(&got, &[Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)]);
}

#[test]
fn snap_moves_points_onto_the_mesh_before_other_tests() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    insert_mesh(&mut app, &[square(-4.0, 4.0, -4.0, 4.0)]);
    provide(
        &mut app,
        "pts",
        vec![Vec3::new(1.0, 0.8, 1.0), Vec3::new(1.0, 5.0, 1.0)],
    );
    // The Distance filter is authored first but runs after the snap: the snapped point is at y 0.
    register(
        &mut app,
        &text(
            "Point",
            "AtAnchor(anchor: Provider(\"pts\"))",
            r#"Distance(id: "low", purpose: Filter, to: Point((1.0, 0.0, 1.0)), max: 0.5),
               Snap(id: "snap", purpose: Filter, up: 1.0, down: 1.0, offset: (0.0, 0.1, 0.0))"#,
            "",
        ),
    );
    let mut req = all(asker);
    req.keep_failed = true;
    let result = run(&mut app, req);
    assert_eq!(result.item_count, 1);
    assert!(
        (result.items[0].position.y - 0.1).abs() < 0.3,
        "{:?}",
        result.items[0]
    );
    let failed = result.items.iter().find(|i| !i.passed).expect("the high point");
    assert_eq!(failed.failed_test.as_deref(), Some("snap"));
}

// § 7 item 18: the probe.
#[cfg(feature = "spatial-query-probe")]
#[test]
fn probe_reruns_after_a_quarter_metre_and_shows_one_test() {
    let mut app = app();
    register(
        &mut app,
        &text(
            "Point",
            "Lattice(half_size: 2.0, spacing: 1.0)",
            r#"Distance(id: "near", purpose: Filter, to: Asker, max: 1.6),
               Distance(id: "nearer", purpose: Filter, to: Asker, max: 1.1)"#,
            "",
        ),
    );
    let probe = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(probe).insert(SpatialQueryProbe {
        pick: Some(Pick::All),
        ..SpatialQueryProbe::new("q")
    });
    let runs = |app: &App| {
        app.world()
            .get::<SpatialQueryProbeState>(probe)
            .map_or(0, |s| s.runs)
    };
    app.update();
    assert_eq!(runs(&app), 1);
    app.update();
    assert_eq!(runs(&app), 1, "no move, no rerun");
    move_to(&mut app, probe, Vec3::new(0.2, 0.0, 0.0));
    app.update();
    assert_eq!(runs(&app), 1, "0.2 m is below the threshold");
    move_to(&mut app, probe, Vec3::new(0.26, 0.0, 0.0));
    app.update();
    assert_eq!(runs(&app), 2, "0.26 m reruns");
    let all_tests = app
        .world()
        .get::<SpatialQueryProbeState>(probe)
        .and_then(|s| s.result.clone())
        .expect("a result");
    assert_eq!(
        all_tests.item_count, 5,
        "within 1.1 m on the lattice: the center and 4 neighbors"
    );

    app.world_mut()
        .get_mut::<SpatialQueryProbe>(probe)
        .expect("probe")
        .only_through_test = Some(0);
    app.update();
    let first_test = app
        .world()
        .get::<SpatialQueryProbeState>(probe)
        .and_then(|s| s.result.clone())
        .expect("a result");
    assert_eq!(first_test.item_count, 9, "within 1.6 m: the 3 x 3 block");
    assert_eq!(first_test.tests.len(), 1);
}

// Concurrency audit 11-03 M3 (ADR-2396): one request whose stage is larger than the tick budget
// must not take the whole budget from the requests behind it in breadth-first mode.
#[test]
fn breadth_first_gives_a_later_request_a_share_when_the_first_stage_is_larger_than_the_budget() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // 41 x 41 = 1681 generated items: one generation stage far larger than the budget.
    register(
        &mut app,
        "(format_version: 1, name: \"big\", item_type: Point, generator: Lattice(half_size: 20.0, spacing: 1.0), tests: [])",
    );
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 1.0, spacing: 1.0)", "", ""),
    );
    manager(&mut app).config.work_units_per_tick = 40;
    let big = manager(&mut app).submit(SpatialQueryRequest::new("big", asker).with_pick(Pick::All));
    let small = manager(&mut app).submit(all(asker));
    tick(&mut app);
    let small_units = manager(&mut app).units_spent(small);
    let finished_small = manager(&mut app).is_finished(small);
    assert!(
        finished_small || small_units.is_some_and(|u| u > 0),
        "the small request advanced (units {small_units:?}, finished {finished_small})"
    );
    assert!(manager(&mut app).units_spent(big).is_some_and(|u| u > 0));
    assert!(manager(&mut app).stats().units_last_tick <= 40);
}

// Concurrency audit 11-03 M2 (ADR-2396): a finished result whose owner is gone and that nobody
// polls is dropped after a grace period instead of being kept for the rest of the session.
#[test]
fn unpolled_results_of_despawned_owners_are_swept() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let keeper = spawn_at(&mut app, Vec3::ONE);
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 1.0, spacing: 1.0)", "", ""),
    );
    let gone = manager(&mut app).submit(all(asker));
    let kept = manager(&mut app).submit(all(keeper));
    tick(&mut app);
    assert!(manager(&mut app).is_finished(gone));
    assert!(manager(&mut app).is_finished(kept));
    app.world_mut().despawn(asker);
    // More than the grace period (600 ticks, ten seconds at 60 Hz).
    for _ in 0..700 {
        tick(&mut app);
    }
    assert!(
        !manager(&mut app).is_finished(gone),
        "a dead owner's unpolled result is swept"
    );
    assert!(
        manager(&mut app).is_finished(kept),
        "a live owner's result waits for its poll"
    );
}

// -- Determinism (audit G) --

/// Finding 2 (audit G): a fixed-step query read the `GlobalTransform` first, which propagates once
/// per frame, so the second fixed step of a frame saw the previous step's pose and the answer
/// depended on the frame rate. The pose must come from the local `Transform` chain.
#[test]
fn second_fixed_step_of_a_frame_sees_the_physics_move() {
    // Run A: the target moved and the transform propagated (the frame snapshot is current).
    let mut early = app();
    let asker = spawn_at(&mut early, Vec3::ZERO);
    spawn_tagged(&mut early, Vec3::new(1.0, 0.0, 0.0), "target");
    register(
        &mut early,
        &text("Entity", "NearbyEntities(tag: Some(\"target\"))", "", ""),
    );
    let a = run(&mut early, all(asker));
    assert_eq!(a.items.len(), 1, "{a:?}");
    // Run B: the target's local `Transform` moved but propagation has not run (the second fixed
    // step of a frame): the query must still see the move, with the same bits as run A.
    let mut late = app();
    let late_asker = spawn_at(&mut late, Vec3::ZERO);
    let target = spawn_tagged(&mut late, Vec3::ZERO, "target");
    late.world_mut()
        .get_mut::<Transform>(target)
        .expect("transform")
        .translation = Vec3::new(1.0, 0.0, 0.0);
    register(
        &mut late,
        &text("Entity", "NearbyEntities(tag: Some(\"target\"))", "", ""),
    );
    let b = run(&mut late, all(late_asker));
    assert_eq!(b.items.len(), 1, "{b:?}");
    assert_eq!(
        b.items[0].position.x.to_bits(),
        a.items[0].position.x.to_bits(),
        "no propagation between the steps must not change the pose"
    );
    assert_eq!(b.items[0].position, Vec3::new(1.0, 0.0, 0.0));
}

/// Finding 4 (audit G): the default `Sample` seed mixed in the owner's entity bits, so the same
/// logical asker drew different samples in two runs that allocated different ids. The seed comes
/// from the owner's spawn-order arrival plus the request id.
#[test]
fn sample_pick_matches_when_only_the_owner_id_changes() {
    use crate::tree::{BehaviorTree, BehaviorTreeDef};
    fn seed_and_pick(eat_id: bool) -> (u64, Vec<Vec3>) {
        let mut app = app();
        if eat_id {
            app.world_mut().spawn_empty();
        }
        let first = app.world_mut().spawn_empty().id();
        let second = app.world_mut().spawn_empty().id();
        let mut registry = crate::tree::BtTaskRegistry::default();
        RunSpatialQuery::new("q", "spot").register(&mut registry, "test::Query");
        let tree = |registry: &crate::tree::BtTaskRegistry| {
            BehaviorTree::compile(&BehaviorTreeDef::leaf("l", "test::Query"), registry).expect("compiles")
        };
        // Same arrival order in every run: `second` receives its tree first even though its id is
        // higher; `eat_id` shifts both ids by one.
        app.world_mut().entity_mut(second).insert(tree(&registry));
        app.world_mut().entity_mut(first).insert(tree(&registry));
        register(
            &mut app,
            &text(
                "Point",
                "Ring(radius: 1.0, count: 3)",
                "",
                "default_pick: Sample(band: 0.3),",
            ),
        );
        app.world_mut()
            .resource_mut::<SpatialQueryManager>()
            .config
            .work_units_per_tick = 0;
        let seed = app
            .world_mut()
            .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
                manager.submit(SpatialQueryRequest::new("q", first));
                manager.tick(world);
                manager
                    .active
                    .first()
                    .map(|active| active.run.spec.seed)
                    .expect("the request is active")
            });
        app.world_mut()
            .resource_mut::<SpatialQueryManager>()
            .config
            .work_units_per_tick = 4096;
        let picked = positions(&run(&mut app, all(first)));
        (seed, picked)
    }
    let (seed_a, pick_a) = seed_and_pick(false);
    let (seed_b, pick_b) = seed_and_pick(true);
    assert_eq!(seed_a, seed_b, "only the owner's id changed; the seed must not");
    assert_eq!(pick_a, pick_b);
}

/// Finding 7 (audit G): entity items came in entity-id order, so `Best` of two equal covers was the
/// lower id, which two runs can allocate differently. The generator emits in tag-arrival order.
#[test]
fn best_of_two_equal_covers_is_the_earlier_spawn() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // Holders get ids in spawn order; the tag arrives in the opposite order, so the earlier
    // arrival has the higher id. Both stand at the same spot with the same (absent) score.
    let low = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    for (holder, position) in [(high, Vec3::new(4.0, 0.0, 0.0)), (low, Vec3::new(4.0, 0.0, 0.0))] {
        app.world_mut().entity_mut(holder).insert((
            Transform::from_translation(position),
            GlobalTransform(Affine3A::from_translation(position)),
            Tags::from_iter_str(["cover"]),
        ));
    }
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"cover\"))", "", ""),
    );
    let result = run(&mut app, SpatialQueryRequest::new("q", asker));
    assert_eq!(result.status, QueryStatus::Success, "{result:?}");
    assert_eq!(
        result.best().and_then(|item| item.entity),
        Some(high),
        "the earlier spawn wins the tie, not the lower id {low}"
    );
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, Med 6): name-only entity sets fell back
/// to the entity id for entities without a `Tags`, so `Best` of two equal covers flipped between
/// runs that allocated the ids differently. A name set emits in `Name` arrival order.
#[test]
fn best_of_two_equal_named_covers_is_the_earlier_name_arrival() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    // The holders get ids in spawn order; the `Name` arrives in the opposite order, so the
    // earlier arrival has the higher id. Neither holder carries a `Tags`.
    let low = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    for (holder, position) in [(high, Vec3::new(4.0, 0.0, 0.0)), (low, Vec3::new(4.0, 0.0, 0.0))] {
        app.world_mut().entity_mut(holder).insert((
            Transform::from_translation(position),
            GlobalTransform(Affine3A::from_translation(position)),
            Name::new("cover"),
        ));
    }
    register(
        &mut app,
        &text("Entity", "NearbyEntities(name: Some(\"cover\"))", "", ""),
    );
    let result = run(&mut app, SpatialQueryRequest::new("q", asker));
    assert_eq!(result.status, QueryStatus::Success, "{result:?}");
    assert_eq!(
        result.best().and_then(|item| item.entity),
        Some(high),
        "the earlier name arrival wins the tie, not the lower id {low}"
    );
}

/// Finding 9 (audit G): ring, wedge and annulus offsets came from the platform libm `sin`/`cos`
/// (and `atan2` for a wedge's facing), whose last bit differs between platforms. The items must
/// come from the deterministic kernel; these are its bits for the eight-point ring.
#[test]
fn ring_of_eight_matches_pinned_bits() {
    // [x bits, z bits] of radius-4 points at i * TAU / 8, from `regia_core::math::det_sin_cos`.
    const PINNED: [[u32; 2]; 8] = [
        [0x4080_0000, 0x0000_0000],
        [0x4035_04f3, 0x4035_04f3],
        [0xb43b_bd2e, 0x4080_0000],
        [0xc035_04f3, 0x4035_04f3],
        [0xc080_0000, 0xb4bb_bd2e],
        [0xc035_04f1, 0xc035_04f5],
        [0x334c_de2e, 0xc080_0000],
        [0x4035_04f7, 0xc035_04ef],
    ];
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(&mut app, &text("Point", "Ring(radius: 4.0, count: 8)", "", ""));
    let result = run(&mut app, all(asker));
    assert_eq!(result.items.len(), 8, "{result:?}");
    for (item, [x_bits, z_bits]) in result.items.iter().zip(PINNED) {
        assert_eq!(item.position.x.to_bits(), x_bits);
        assert_eq!(item.position.z.to_bits(), z_bits);
    }
}

// ---- despawned owners and failure reasons (re-landed lost work, 2026-11-09) ----

#[test]
fn forget_owner_drops_in_flight_and_finished_results_of_that_owner_only() {
    let mut app = app();
    let gone = spawn_at(&mut app, Vec3::ZERO);
    let kept = spawn_at(&mut app, Vec3::ONE);
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 20.0, spacing: 1.0)", "", ""),
    );
    manager(&mut app).config.work_units_per_tick = 10;
    let in_flight = manager(&mut app).submit(all(gone));
    let other = manager(&mut app).submit(all(kept));
    tick(&mut app);
    manager(&mut app).abort(other);
    let finished_gone = manager(&mut app).submit(all(gone));
    manager(&mut app).abort(finished_gone);
    assert!(manager(&mut app).is_finished(finished_gone));

    assert_eq!(manager(&mut app).forget_owner(gone), 2);
    assert!(!manager(&mut app).is_active(in_flight));
    assert!(!manager(&mut app).is_finished(in_flight));
    assert!(!manager(&mut app).is_finished(finished_gone));
    assert!(
        manager(&mut app).is_finished(other),
        "another owner's result stays"
    );
    let _ = manager(&mut app).poll(other);
}

#[test]
fn a_run_spatial_query_failure_names_its_reason_on_the_blackboard() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"none\"))", "", ""),
    );
    let mut task = RunSpatialQuery::new("q", "target");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Failure);
    let bb = app.world().get::<Blackboard>(asker).expect("bb");
    assert!(bb.contains("target_error"));
    assert!(!bb.contains("target"));
}

#[test]
fn a_finished_query_survives_a_tick_without_a_blackboard() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let pickup = spawn_tagged(&mut app, Vec3::new(2.0, 0.0, 0.0), "pickup");
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"pickup\"))", "", ""),
    );
    let mut task = RunSpatialQuery::new("q", "target");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Failure);
    let id = task.request().expect("the request is kept");
    assert!(manager(&mut app).is_finished(id), "the result was not consumed");
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Success);
    let bb = app.world().get::<Blackboard>(asker).expect("bb");
    assert_eq!(bb.get("target"), Some(&BbValue::Entity(pickup.to_bits())));
}

/// An owner the behavior-tree spawn-order table never saw must keep its entity bits. XORing the
/// index into those bits cancels the index (`to_bits` is index in the low half), so two owners of
/// the same generation share a seed.
#[test]
fn audfix_untracked_owner_seed_keeps_entity_bits() {
    let mut app = app();
    assert!(
        regia_ecs::spawn_order::track_spawn_order::<crate::BehaviorTree>(app.world_mut()),
        "behavior-tree spawn order must be tracking"
    );
    let first = app.world_mut().spawn_empty().id();
    let second = app.world_mut().spawn_empty().id();
    let first_key = owner_seed_key(app.world(), first);
    let second_key = owner_seed_key(app.world(), second);
    assert_eq!(first_key, first.to_bits(), "owner {first:?}");
    assert_eq!(second_key, second.to_bits(), "owner {second:?}");
    assert_ne!(first_key, second_key);
}

/// corr-low finding 13: the tree tick takes `BehaviorTree` out of the owner while a leaf runs
/// (take-run-reinsert), so an instant query asked from inside the leaf found no behavior-tree
/// spawn-order entry and seeded from the owner's entity bits — which two runs of the same level
/// allocate differently. The seed prefers the blackboard's arrival, which the tick never takes.
#[test]
fn instant_seed_from_inside_a_leaf_is_not_the_entity_bits() {
    use crate::tree::{BehaviorTree, BehaviorTreeDef};
    let mut app = app();
    // SpatialQueryPlugin already tracks `BehaviorTree`; AiPlugin additionally tracks `Blackboard`
    // (see `owner_seed_key`). The harness app does not add AiPlugin, so install it the same way.
    assert!(
        regia_ecs::spawn_order::track_spawn_order::<Blackboard>(app.world_mut()),
        "blackboard spawn order must be tracking"
    );
    let mut registry = crate::tree::BtTaskRegistry::default();
    RunSpatialQuery::new("q", "spot").register(&mut registry, "test::Query");
    let tree =
        BehaviorTree::compile(&BehaviorTreeDef::leaf("l", "test::Query"), &registry).expect("compiles");
    let owner = app.world_mut().spawn((Blackboard::default(), tree)).id();
    let board_seq = app
        .world()
        .resource::<regia_ecs::spawn_order::SpawnOrder<Blackboard>>()
        .get(owner);
    // The take-run-reinsert window: the tree is out of the entity while its leaf runs.
    let taken = app.world_mut().entity_mut(owner).take::<crate::BehaviorTree>();
    assert!(taken.is_some(), "the tree is taken for the leaf");
    let key = owner_seed_key(app.world(), owner);
    assert_eq!(
        Some(key),
        board_seq,
        "inside the leaf the seed comes from the blackboard arrival"
    );
    assert_ne!(
        key,
        owner.to_bits(),
        "the entity-bits fallback is not deterministic across runs"
    );
}

/// A success after a failure replaces the failure: the reason written by the failed run must not
/// stay on the blackboard next to the new winner.
#[test]
fn a_run_spatial_query_success_clears_an_earlier_failure_reason() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    register(
        &mut app,
        &text("Entity", "NearbyEntities(tag: Some(\"pickup\"))", "", ""),
    );
    let mut task = RunSpatialQuery::new("q", "target");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Failure);
    assert!(
        app.world()
            .get::<Blackboard>(asker)
            .expect("bb")
            .contains("target_error")
    );
    let pickup = spawn_tagged(&mut app, Vec3::new(2.0, 0.0, 0.0), "pickup");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Success);
    let bb = app.world().get::<Blackboard>(asker).expect("bb");
    assert_eq!(bb.get("target"), Some(&BbValue::Entity(pickup.to_bits())));
    assert!(
        !bb.contains("target_error"),
        "the earlier failure reason survived a success"
    );
}

/// Deep-zai lower note (generator numerics): a `NaN` literal in `spacing` or `radius` has no
/// finiteness rule — a lattice silently emitted one item (the NaN `k` casts to 0) and a ring
/// emitted NaN positions. Both must end the run `failed` with `sq_bad_param` instead.
#[test]
fn non_finite_generator_numbers_fail_the_query() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);

    // A literal: `spacing: NaN`.
    register(
        &mut app,
        &text("Point", "Lattice(half_size: 2.0, spacing: NaN)", "", ""),
    );
    let result = run(&mut app, all(asker));
    assert_eq!(result.status, QueryStatus::Failed, "lattice spacing NaN");
    assert_eq!(
        result.code,
        Some("sq_bad_param"),
        "{}",
        result.message.unwrap_or_default()
    );
    assert!(result.items.is_empty());

    // A literal radius: NaN positions otherwise.
    register(&mut app, &text("Point", "Ring(radius: NaN, count: 4)", "", ""));
    let result = run(&mut app, all(asker));
    assert_eq!(result.status, QueryStatus::Failed, "ring radius NaN");
    assert_eq!(
        result.code,
        Some("sq_bad_param"),
        "{}",
        result.message.unwrap_or_default()
    );
    assert!(
        result.items.iter().all(|item| item.position.is_finite()),
        "no NaN position may leave the run"
    );

    // A parameter resolved to NaN: the same rule at the same choke point.
    register(
        &mut app,
        &text(
            "Point",
            "Ring(radius: ParamF32(\"r\"), count: 4)",
            "",
            "params: { \"r\": Float(1.0) },",
        ),
    );
    let result = run(&mut app, all(asker).with_param("r", ParamValue::Float(f32::NAN)));
    assert_eq!(result.status, QueryStatus::Failed, "ring radius param NaN");
    assert_eq!(result.code, Some("sq_bad_param"));
}

/// Deep-zai lower note (`spatial_query/bt.rs`): `Pick::All` with zero survivors is a success
/// with no message, so the behavior-tree leaf wrote `<key>_error = "success"`. The leaf must
/// report a real reason (the run's own "no item survived the filters" wording).
#[test]
fn pick_all_with_zero_survivors_reports_a_real_reason_on_the_blackboard() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    app.world_mut().entity_mut(asker).insert(Blackboard::default());
    // A ring of radius 0 generates no points: Pick::All succeeds with zero items.
    register(
        &mut app,
        &text("Point", "Ring(radius: 0.0, count: 4)", "", "default_pick: All,"),
    );
    let result = run(&mut app, SpatialQueryRequest::new("q", asker));
    assert_eq!(result.status, QueryStatus::Success, "empty All stays a success");
    assert!(result.items.is_empty());

    let mut task = RunSpatialQuery::new("q", "spot");
    bt_tick(&mut app, &mut task, asker);
    tick(&mut app);
    assert_eq!(bt_tick(&mut app, &mut task, asker), BtStatus::Failure);
    let bb = app.world().get::<Blackboard>(asker).expect("bb");
    let reason = bb.get("spot_error").expect("the failure writes a reason");
    let BbValue::String(text) = reason else {
        panic!("the reason is a string, got {reason:?}");
    };
    assert_ne!(
        text, "success",
        "the reason must not name the status it failed on"
    );
    assert!(text.contains("no item"), "a real reason, got {text}");
}
