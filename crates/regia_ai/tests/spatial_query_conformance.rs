// SPDX-License-Identifier: MPL-2.0
//! Spec 33 conformance (`docs/specs/33-spatial-queries.md`): the parts of §§ 3.2-3.6 the acceptance
//! tests in `src/spatial_query/tests.rs` do not reach (Distance modes, Facing `planar`/`abs`,
//! `drop_to_ground`, cast volumes and `blocking_only`, Clearance `skip_asker`, `Blackboard` anchors,
//! Route `FromAnchor` and RouteAll `scan_limit`, the `max_tests` limit and the two warnings). Fixed
//! inputs and exact comparisons; no golden, GPU or wall clock.

use regia_ai::spatial_query::{
    AnchorData, AnchorProvider, Pick, QueryStatus, SpatialQueryAsset, SpatialQueryManager,
    SpatialQueryPlugin, SpatialQueryRequest, SpatialQueryResult,
};
use regia_ai::{BbValue, Blackboard};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_nav::NavMeshBuilder;
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::{GlobalTransform, Transform};

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

fn spawn_box(app: &mut App, center: Vec3, half: Vec3, sensor: bool) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(center),
            GlobalTransform(Affine3A::from_translation(center)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                sensor,
                ..Collider::default()
            },
        ))
        .id()
}

/// Steps until the physics world holds every collider spawned so far.
fn settle(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

fn text(generator: &str, tests: &str) -> String {
    format!("(format_version: 1, name: \"q\", item_type: Point, generator: {generator}, tests: [{tests}])")
}

fn manager(app: &mut App) -> Mut<'_, SpatialQueryManager> {
    app.world_mut().resource_mut::<SpatialQueryManager>()
}

fn register(app: &mut App, ron: &str) {
    manager(app)
        .register_template_text(ron)
        .unwrap_or_else(|e| panic!("template loads: {e}\n{ron}"));
}

fn run(app: &mut App, req: SpatialQueryRequest) -> SpatialQueryResult {
    app.world_mut()
        .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.run_instant(world, req))
}

fn all(owner: Entity) -> SpatialQueryRequest {
    SpatialQueryRequest::new("q", owner).with_pick(Pick::All)
}

/// The survivors in generation order (`All` sorts by score; ties keep generation order, and every
/// query here without score tests ties at 0).
fn survivors(result: &SpatialQueryResult) -> Vec<Vec3> {
    let mut items: Vec<_> = result.items.iter().filter(|i| i.passed).collect();
    items.sort_by_key(|i| i.generation_index);
    items.iter().map(|i| i.position).collect()
}

fn assert_points(got: &[Vec3], want: &[Vec3]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!(
            (*g - *w).abs().max_element() <= 1e-4,
            "got {got:?}, want {want:?}"
        );
    }
}

struct Points(Vec<Vec3>);

impl AnchorProvider for Points {
    fn provide(&self, _world: &World, _asker: Entity) -> AnchorData {
        AnchorData {
            points: self.0.clone(),
            entities: Vec::new(),
        }
    }
}

fn provide(app: &mut App, points: Vec<Vec3>) {
    manager(app).register_provider("pts", Box::new(Points(points)));
}

/// The raw value of the single score test `id` on each survivor (generation order), recovered from
/// its contribution: with `clamp_min: Fixed(-10)`, `clamp_max: Fixed(10)`, `Linear`, weight 1 the
/// contribution is `(v + 10) / 20` (§ 3.5 steps 1-6).
fn raw_values(result: &SpatialQueryResult, id: &str) -> Vec<f32> {
    let mut items: Vec<_> = result.items.iter().filter(|i| i.passed).collect();
    items.sort_by_key(|i| i.generation_index);
    items
        .iter()
        .map(|i| {
            let (_, c) = i
                .contributions
                .iter()
                .find(|(t, _)| t == id)
                .unwrap_or_else(|| panic!("no contribution `{id}` on {i:?}"));
            c * 20.0 - 10.0
        })
        .collect()
}

const RAW_SCORE: &str =
    "score: (shape: Linear, weight: 1.0, clamp_min: Fixed(-10.0), clamp_max: Fixed(10.0))";

fn assert_close(got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() <= 1e-4, "got {got:?}, want {want:?}");
    }
}

// § 3.4 Distance: `Full3d`, `Planar` (ignores height), `Vertical` (signed item minus anchor height),
// `AbsVertical`.
#[test]
fn distance_modes_measure_full_planar_signed_and_absolute_height() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    provide(
        &mut app,
        vec![Vec3::new(3.0, 4.0, 0.0), Vec3::new(0.0, -2.0, 0.0)],
    );
    for (mode, want) in [
        ("Full3d", [5.0, 2.0]),
        ("Planar", [3.0, 0.0]),
        ("Vertical", [4.0, -2.0]),
        ("AbsVertical", [4.0, 2.0]),
    ] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Distance(id: \"d\", purpose: Score, to: Point((0.0, 0.0, 0.0)), mode: {mode}, {RAW_SCORE})"
                ),
            ),
        );
        let result = run(&mut app, all(asker));
        assert_eq!(result.status, QueryStatus::Success);
        assert_close(&raw_values(&result, "d"), &want);
    }
}

// § 3.4 Facing: the dot product of two unit directions; `planar` drops the height first and `abs`
// takes the absolute value.
#[test]
fn facing_planar_and_abs() {
    let mut app = app();
    // The asker's forward is -Z (identity rotation).
    let asker = spawn_at(&mut app, Vec3::ZERO);
    provide(
        &mut app,
        vec![Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 5.0, -5.0)],
    );
    let h = 0.5f32.sqrt();
    for (flags, want) in [
        ("", [-1.0, h]),
        ("abs: true,", [1.0, h]),
        ("planar: true,", [-1.0, 1.0]),
    ] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Facing(id: \"f\", purpose: Score, a: Forward(Asker), b: Between(Asker, Candidate()), {flags} {RAW_SCORE})"
                ),
            ),
        );
        assert_close(&raw_values(&run(&mut app, all(asker)), "f"), &want);
    }
}

// § 3.3: `drop_to_ground: h` lowers each point by a downward trace; a point with no ground under it
// within the trace is dropped (ADR-5633 records the drop).
#[test]
fn drop_to_ground_lowers_points_onto_the_floor() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::new(0.0, 0.0, -3.0));
    // Floor top at y = 0 over x, z in -5..5.
    spawn_box(
        &mut app,
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(5.0, 0.5, 5.0),
        false,
    );
    settle(&mut app);
    provide(
        &mut app,
        vec![
            Vec3::new(1.0, 2.0, 1.0),
            Vec3::new(-2.0, 0.4, 0.0),
            Vec3::new(20.0, 2.0, 0.0),
        ],
    );
    register(
        &mut app,
        &text("AtAnchor(anchor: Provider(\"pts\"), drop_to_ground: 3.0)", ""),
    );
    let result = run(&mut app, all(asker));
    assert_eq!(result.status, QueryStatus::Success);
    assert_points(
        &survivors(&result),
        &[Vec3::new(1.0, 0.0, 1.0), Vec3::new(-2.0, 0.0, 0.0)],
    );
    // Without the option the points stay where the generator put them.
    register(&mut app, &text("AtAnchor(anchor: Provider(\"pts\"))", ""));
    assert_eq!(survivors(&run(&mut app, all(asker))).len(), 3);
}

// § 3.4 Cast: `Sphere`, `Box` and `Capsule` sweep a volume (`shapecast`), so a wall the line passes
// beside blocks a wide volume and not a thin one.
#[test]
fn cast_volumes_sweep_and_the_line_does_not() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::new(-20.0, 0.0, 0.0));
    // A post at x 0.5..0.9 halfway along the cast from (0, 1, 0) to (0, 1, 10).
    spawn_box(
        &mut app,
        Vec3::new(0.7, 1.0, 5.0),
        Vec3::new(0.2, 1.0, 0.2),
        false,
    );
    settle(&mut app);
    provide(&mut app, vec![Vec3::new(0.0, 1.0, 0.0)]);
    for (shape, clear) in [
        ("Line", true),
        ("Sphere(0.3)", true),
        ("Sphere(1.0)", false),
        ("Box((1.0, 0.2, 0.2))", false),
        ("Box((0.2, 0.2, 0.2))", true),
        ("Capsule(1.0, 0.5)", false),
        ("Capsule(0.2, 0.5)", true),
    ] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Cast(id: \"c\", purpose: Filter, from: Candidate(), to: Point((0.0, 1.0, 10.0)), shape: {shape}, want_hit: false)"
                ),
            ),
        );
        let kept = survivors(&run(&mut app, all(asker))).len();
        assert_eq!(kept == 1, clear, "{shape}: kept {kept}");
    }
}

// § 3.4 Cast and Clearance `blocking_only`: a sensor collider blocks unless `blocking_only` is set.
#[test]
fn blocking_only_ignores_sensors() {
    let mut app = physics_app();
    let asker = spawn_at(&mut app, Vec3::new(-20.0, 0.0, 0.0));
    spawn_box(&mut app, Vec3::new(0.0, 1.0, 5.0), Vec3::new(1.0, 1.0, 0.2), true);
    settle(&mut app);
    provide(&mut app, vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 1.0, 5.0)]);
    for (blocking_only, want) in [(false, 0), (true, 1)] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Cast(id: \"c\", purpose: Filter, from: Candidate(), to: Point((0.0, 1.0, 10.0)), shape: Line, blocking_only: {blocking_only}, want_hit: false)"
                ),
            ),
        );
        // Both points' lines cross (or start inside) the sensor.
        let kept = survivors(&run(&mut app, all(asker)));
        assert_eq!(
            kept.len(),
            2 * want,
            "cast blocking_only {blocking_only}: {kept:?}"
        );
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Clearance(id: \"room\", purpose: Filter, shape: Sphere(0.3), blocking_only: {blocking_only})"
                ),
            ),
        );
        // Only the second point is inside the sensor.
        let kept = survivors(&run(&mut app, all(asker)));
        assert_eq!(
            kept.len(),
            1 + want,
            "clearance blocking_only {blocking_only}: {kept:?}"
        );
    }
}

// § 3.4 Clearance `skip_asker` (default true): the asker's own collider does not count.
#[test]
fn clearance_skips_the_askers_collider_by_default() {
    let mut app = physics_app();
    let asker = spawn_box(
        &mut app,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.4, 1.0, 0.4),
        false,
    );
    settle(&mut app);
    for (flag, kept) in [("", 1), ("skip_asker: true,", 1), ("skip_asker: false,", 0)] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Asker)",
                &format!("Clearance(id: \"room\", purpose: Filter, shape: Sphere(0.3), {flag})"),
            ),
        );
        assert_eq!(survivors(&run(&mut app, all(asker))).len(), kept, "`{flag}`");
    }
}

// § 3.2 `Blackboard(key)`: a vec3 or an entity on the asker's blackboard; a missing key is an empty
// anchor (no value, no error).
#[test]
fn blackboard_anchors_read_a_vec3_and_an_entity() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let foe = spawn_at(&mut app, Vec3::new(0.0, 0.0, 7.0));
    let mut bb = Blackboard::default();
    bb.insert("spot", BbValue::Vec3(Vec3::new(5.0, 0.0, 0.0)));
    bb.insert("foe", BbValue::Entity(foe.to_bits()));
    app.world_mut().entity_mut(asker).insert(bb);
    for (key, want) in [
        ("spot", vec![Vec3::new(5.0, 0.0, 0.0)]),
        ("foe", vec![Vec3::new(0.0, 0.0, 7.0)]),
        ("absent", vec![]),
    ] {
        register(
            &mut app,
            &text(&format!("AtAnchor(anchor: Blackboard(\"{key}\"))"), ""),
        );
        let result = run(&mut app, all(asker));
        assert_eq!(result.status, QueryStatus::Success, "{key}");
        assert_points(&survivors(&result), &want);
    }
    // A filter whose anchor is the missing key fails every item (reason `no_anchor`, § 3.2).
    register(
        &mut app,
        &text(
            "AtAnchor(anchor: Asker)",
            "Distance(id: \"d\", purpose: Filter, to: Blackboard(\"absent\"), max: 100.0)",
        ),
    );
    let mut req = all(asker);
    req.keep_failed = true;
    let result = run(&mut app, req);
    assert_eq!(result.item_count, 0);
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].failed_reason.as_deref(), Some("no_anchor"));
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

// § 3.4 Route `direction: FromAnchor` measures the path from the anchor to the item (the same length
// on an undirected mesh), and RouteAll's `scan_limit` skips items farther from the anchor than the
// limit (unreachable: dropped with `drop_unreachable`); the default limit (1.5 times the largest
// item-to-anchor distance) keeps every item.
#[test]
fn route_from_anchor_and_route_all_scan_limit() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    let triangles: Vec<[Vec3; 3]> = square(-4.0, 4.0, -4.0, 4.0).into_iter().collect();
    let mesh = NavMeshBuilder::default()
        .build(&triangles)
        .expect("the test mesh builds");
    app.insert_resource(mesh);
    provide(&mut app, vec![Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)]);
    let mut lengths = Vec::new();
    for direction in ["ToAnchor", "FromAnchor"] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "Route(id: \"r\", purpose: Score, to: Asker, direction: {direction}, measure: Length, {RAW_SCORE})"
                ),
            ),
        );
        lengths.push(raw_values(&run(&mut app, all(asker)), "r"));
    }
    assert_close(&lengths[0], &[1.0, 3.0]);
    assert_close(&lengths[1], &lengths[0]);

    for (limit, want) in [
        ("scan_limit: 2.0,", vec![Vec3::new(1.0, 0.0, 0.0)]),
        ("", vec![Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)]),
    ] {
        register(
            &mut app,
            &text(
                "AtAnchor(anchor: Provider(\"pts\"))",
                &format!(
                    "RouteAll(id: \"ra\", purpose: Filter, to: Asker, measure: Length, drop_unreachable: true, {limit} max: 100.0)"
                ),
            ),
        );
        assert_points(&survivors(&run(&mut app, all(asker))), &want);
    }
}

// § 3.6 `max_tests`: an asset with more tests than the limit does not load (`sq_unreadable`).
#[test]
fn more_tests_than_max_tests_is_unreadable() {
    let three = text(
        "Ring(radius: 1.0, count: 2)",
        "Distance(id: \"a\", purpose: Score, to: Asker), Distance(id: \"b\", purpose: Score, to: Asker), Distance(id: \"c\", purpose: Score, to: Asker)",
    );
    assert!(SpatialQueryAsset::parse_with_limit(&three, 3).is_ok());
    let error = SpatialQueryAsset::parse_with_limit(&three, 2).expect_err("over the limit");
    assert_eq!(error.code(), "sq_unreadable");
    let mut app = app();
    manager(&mut app).config.max_tests = 2;
    let error = manager(&mut app)
        .register_template_text(&three)
        .expect_err("the manager applies its limit");
    assert_eq!(error.code(), "sq_unreadable");
}

// § 3.6 `active_warn_threshold` and `slow_warn_units`: counted, never timed; both land in
// `stats().warnings`.
#[test]
fn active_and_slow_warnings_are_counted_and_reported() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Lattice(half_size: 2.0, spacing: 1.0)",
            "Distance(id: \"d\", purpose: Score, to: Asker)",
        ),
    );
    // 25 generated items + 25 evaluations = 50 units.
    manager(&mut app).config.slow_warn_units = 50;
    let result = run(&mut app, all(asker));
    assert_eq!(result.units, 50);
    assert!(manager(&mut app).stats().warnings.is_empty(), "50 is not over 50");
    manager(&mut app).config.slow_warn_units = 49;
    run(&mut app, all(asker));
    let warnings = manager(&mut app).stats().warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("slow_warn_units"), "{warnings:?}");

    manager(&mut app).config.active_warn_threshold = 1;
    let first = manager(&mut app).submit(all(asker));
    assert_eq!(
        manager(&mut app).stats().warnings.len(),
        1,
        "one active is not over 1"
    );
    let second = manager(&mut app).submit(all(asker));
    let warnings = manager(&mut app).stats().warnings;
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[1].contains("active_warn_threshold"), "{warnings:?}");
    manager(&mut app).fail_all();
    assert_eq!(
        manager(&mut app).poll(first).map(|r| r.status),
        Some(QueryStatus::Failed)
    );
    assert_eq!(
        manager(&mut app).poll(second).map(|r| r.status),
        Some(QueryStatus::Failed)
    );
}

// § 4.2 probe `max_units` (through `run_limited`, which the probe calls): the preview stops,
// `aborted`, once it passes the limit; 0 means no limit.
#[test]
fn run_limited_stops_past_max_units() {
    let mut app = app();
    let asker = spawn_at(&mut app, Vec3::ZERO);
    register(
        &mut app,
        &text(
            "Lattice(half_size: 2.0, spacing: 1.0)",
            "Distance(id: \"d\", purpose: Score, to: Asker)",
        ),
    );
    let limited = app
        .world_mut()
        .resource_scope(|world, mut m: Mut<SpatialQueryManager>| m.run_limited(world, all(asker), None, 30));
    assert_eq!(limited.status, QueryStatus::Aborted);
    assert!(limited.units < 50, "stopped early: {}", limited.units);
    let unlimited = app
        .world_mut()
        .resource_scope(|world, mut m: Mut<SpatialQueryManager>| m.run_limited(world, all(asker), None, 0));
    assert_eq!(unlimited.status, QueryStatus::Success);
    assert_eq!(unlimited, run(&mut app, all(asker)), "0 = the instant run");
}
