// SPDX-License-Identifier: MPL-2.0
//! Custom spatial-query generators and tests (`GeneratorImpl`, `TestImpl`, spec 33 § 8, ADR-2709):
//! fixed inputs and exact comparisons, no wall clock.

use regia_ai::spatial_query::{
    GeneratedItem, GeneratorImpl, GeneratorInput, ParamValue, Pick, QueryStatus,
    SPATIAL_QUERY_FORMAT_VERSION, SpatialQueryAsset, SpatialQueryManager, SpatialQueryPlugin,
    SpatialQueryRequest, SpatialQueryResult, TestImpl, TestInput, TestValue, migrate_ron_v1_to_v2,
    spatial_query_migration_sources,
};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::components::Tags;
use regia_scene::{GlobalTransform, Transform};

/// The same points as `Ring(radius, count)` around each center, in the same order.
struct RingGenerator;

impl GeneratorImpl for RingGenerator {
    fn generate(&self, input: &GeneratorInput<'_>) -> Vec<GeneratedItem> {
        let radius = input.args.get("radius").copied().unwrap_or(0.0);
        let count = input.args.get("count").copied().unwrap_or(0.0).round() as usize;
        let mut out = Vec::new();
        for center in input.centers {
            for i in 0..count {
                let angle = i as f32 * std::f32::consts::TAU / count as f32;
                out.push(GeneratedItem::Point(
                    *center + Vec3::new(angle.cos(), 0.0, angle.sin()) * radius,
                ));
            }
        }
        out
    }
}

/// The same value as `Distance(mode: Full3d)`.
struct DistanceTest;

impl TestImpl for DistanceTest {
    fn evaluate(&self, input: &TestInput<'_>) -> TestValue {
        TestValue::Float(input.anchor.map(|anchor| (anchor - input.position).length()))
    }
}

/// True when the item is farther along +X than the `x` argument.
struct EastOf;

impl TestImpl for EastOf {
    fn evaluate(&self, input: &TestInput<'_>) -> TestValue {
        TestValue::Bool(input.position.x > input.args.get("x").copied().unwrap_or(0.0))
    }
}

/// Every entity with the `crate` tag, in entity id order.
struct Crates;

impl GeneratorImpl for Crates {
    fn generate(&self, input: &GeneratorInput<'_>) -> Vec<GeneratedItem> {
        let mut found: Vec<Entity> = input
            .world
            .try_query::<(Entity, &Tags)>()
            .map(|mut q| {
                q.iter(input.world)
                    .filter(|(_, tags)| tags.contains("crate"))
                    .map(|(e, _)| e)
                    .collect()
            })
            .unwrap_or_default();
        found.sort_by_key(|e| (e.index_u32(), e.to_bits()));
        found.into_iter().map(GeneratedItem::Entity).collect()
    }
}

/// A fixed answer, for the failure paths.
struct Fixed(Vec<GeneratedItem>);

impl GeneratorImpl for Fixed {
    fn generate(&self, _input: &GeneratorInput<'_>) -> Vec<GeneratedItem> {
        self.0.clone()
    }
}

/// Infinite for items on the +X side, the x coordinate otherwise.
struct InfiniteEast;

impl TestImpl for InfiniteEast {
    fn evaluate(&self, input: &TestInput<'_>) -> TestValue {
        TestValue::Float(Some(if input.position.x > 0.5 {
            f32::NEG_INFINITY
        } else {
            input.position.x
        }))
    }
}

struct Returns(TestValue);

impl TestImpl for Returns {
    fn evaluate(&self, _input: &TestInput<'_>) -> TestValue {
        self.0
    }
}

fn spawn_at(app: &mut App, pos: Vec3, tag: Option<&str>) -> Entity {
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(pos),
            GlobalTransform(Affine3A::from_translation(pos)),
        ))
        .id();
    if let Some(tag) = tag {
        app.world_mut()
            .entity_mut(entity)
            .insert(Tags::from_iter_str([tag]));
    }
    entity
}

/// An app with the custom implementations registered, an asker at the origin and three players.
fn fixture() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SpatialQueryPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    let asker = spawn_at(&mut app, Vec3::ZERO, None);
    spawn_at(&mut app, Vec3::new(9.0, 0.0, 2.0), Some("player"));
    spawn_at(&mut app, Vec3::new(-3.0, 1.0, 7.0), Some("player"));
    spawn_at(&mut app, Vec3::new(4.0, 0.0, -6.0), Some("player"));
    let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
    manager.register_generator("ring", Box::new(RingGenerator));
    manager.register_test("distance", Box::new(DistanceTest));
    manager.register_test("east_of", Box::new(EastOf));
    (app, asker)
}

fn register(app: &mut App, ron: &str) -> String {
    app.world_mut()
        .resource_mut::<SpatialQueryManager>()
        .register_template_text(ron)
        .expect("the query loads")
}

fn run(app: &mut App, request: SpatialQueryRequest) -> SpatialQueryResult {
    app.world_mut()
        .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.run_instant(world, request))
}

/// Everything a pick is made of, compared exactly.
fn picks(result: &SpatialQueryResult) -> Vec<(u32, u32, u32, u32, usize)> {
    result
        .items
        .iter()
        .map(|i| {
            (
                i.position.x.to_bits(),
                i.position.y.to_bits(),
                i.position.z.to_bits(),
                i.score.to_bits(),
                i.generation_index,
            )
        })
        .collect()
}

const BUILT_IN: &str = r#"(
    format_version: 2,
    name: "built_in",
    item_type: Point,
    params: { "radius": Float(12.0) },
    generator: Ring(center: Asker, radius: ParamF32("radius"), count: 48),
    tests: [
        Distance(id: "far_enough", purpose: Filter, to: Entity(tag: "player"), min: 5.0, require: Every),
        Distance(id: "near", purpose: Score, to: Entity(tag: "player"), combine: Lowest,
                 score: (shape: Quadratic, weight: -2.0)),
        Distance(id: "spread", purpose: Both, to: Point((1.0, 0.0, 1.0)), max: 13.0,
                 score: (shape: Root, weight: 0.5, clamp_min: Fixed(2.0), clamp_max: FilterBound)),
    ],
)"#;

const CUSTOM: &str = r#"(
    format_version: 2,
    name: "custom",
    item_type: Point,
    params: { "radius": Float(12.0) },
    generator: Custom(name: "ring", center: Asker, args: { "radius": ParamF32("radius"), "count": 48.0 }),
    tests: [
        Custom(id: "far_enough", purpose: Filter, name: "distance", value: Float, to: Entity(tag: "player"),
               min: 5.0, require: Every),
        Custom(id: "near", purpose: Score, name: "distance", value: Float, to: Entity(tag: "player"),
               combine: Lowest, score: (shape: Quadratic, weight: -2.0)),
        Custom(id: "spread", purpose: Both, name: "distance", value: Float, to: Point((1.0, 0.0, 1.0)), max: 13.0,
               score: (shape: Root, weight: 0.5, clamp_min: Fixed(2.0), clamp_max: FilterBound)),
    ],
)"#;

#[test]
fn a_custom_generator_and_test_pick_what_the_equivalent_built_ins_pick() {
    let (mut app, asker) = fixture();
    let built_in = register(&mut app, BUILT_IN);
    let custom = register(&mut app, CUSTOM);
    for pick in [
        Pick::All,
        Pick::Best,
        Pick::Sample { band: 0.25 },
        Pick::Sample { band: 1.0 },
    ] {
        for seed in [1u64, 2, 3, 99] {
            let a = run(
                &mut app,
                SpatialQueryRequest::new(built_in.clone(), asker)
                    .with_pick(pick)
                    .with_seed(seed),
            );
            let b = run(
                &mut app,
                SpatialQueryRequest::new(custom.clone(), asker)
                    .with_pick(pick)
                    .with_seed(seed),
            );
            assert_eq!(a.status, QueryStatus::Success, "{:?}", a.message);
            assert_eq!(b.status, QueryStatus::Success, "{:?}", b.message);
            assert!(!a.items.is_empty());
            assert_eq!(picks(&a), picks(&b), "{pick:?} seed {seed}");
            assert_eq!(a.item_count, b.item_count);
            assert_eq!(a.units, b.units, "the same work is counted");
            // The filters did drop something, so the filter half of the row was exercised.
            assert!(a.item_count < 48);
            let counts =
                |r: &SpatialQueryResult| r.tests.iter().map(|t| (t.input, t.output)).collect::<Vec<_>>();
            assert_eq!(counts(&a), counts(&b));
            assert_eq!(b.tests[0].kind, "Custom");
        }
    }
}

#[test]
fn async_runs_of_a_custom_query_match_the_instant_run() {
    let (mut app, asker) = fixture();
    let custom = register(&mut app, CUSTOM);
    let instant = run(
        &mut app,
        SpatialQueryRequest::new(custom.clone(), asker)
            .with_pick(Pick::All)
            .with_seed(7),
    );
    let id = {
        let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
        manager.config.work_units_per_tick = 16;
        manager.submit(
            SpatialQueryRequest::new(custom, asker)
                .with_pick(Pick::All)
                .with_seed(7),
        )
    };
    let mut result = None;
    for _ in 0..1000 {
        app.world_mut()
            .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.tick(world));
        let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
        assert!(manager.stats().units_last_tick <= 16);
        result = manager.poll(id);
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("the request finishes");
    assert_eq!(picks(&instant), picks(&result));
}

#[test]
fn a_supplied_parameter_reaches_a_custom_arg_and_a_missing_one_is_missing_param() {
    let (mut app, asker) = fixture();
    let name = register(
        &mut app,
        r#"(format_version: 2, name: "p", item_type: Point,
            generator: Custom(name: "ring", args: { "radius": ParamF32("r"), "count": 4.0 }))"#,
    );
    let missing = run(&mut app, SpatialQueryRequest::new(name.clone(), asker));
    assert_eq!(missing.status, QueryStatus::MissingParam);
    assert!(missing.message.as_deref().is_some_and(|m| m.contains("`r`")));
    let good = run(
        &mut app,
        SpatialQueryRequest::new(name, asker)
            .with_pick(Pick::All)
            .with_param("r", ParamValue::Float(3.0)),
    );
    assert_eq!(good.status, QueryStatus::Success);
    assert_eq!(good.items.len(), 4);
    assert!((good.items[0].position - Vec3::new(3.0, 0.0, 0.0)).length() < 1e-5);
}

#[test]
fn a_bool_custom_test_filters_against_want_and_reads_its_args() {
    let (mut app, asker) = fixture();
    let name = register(
        &mut app,
        r#"(format_version: 2, name: "b", item_type: Point, default_pick: All,
            generator: Ring(radius: 2.0, count: 4),
            tests: [Custom(id: "east", purpose: Filter, name: "east_of", args: { "x": 1.0 })])"#,
    );
    let kept = run(&mut app, SpatialQueryRequest::new(name, asker));
    assert_eq!(kept.items.len(), 1);
    assert!(kept.items[0].position.x > 1.0);
    let inverted = register(
        &mut app,
        r#"(format_version: 2, name: "b2", item_type: Point, default_pick: All,
            generator: Ring(radius: 2.0, count: 4),
            tests: [Custom(id: "east", purpose: Filter, name: "east_of", want: false, args: { "x": 1.0 })])"#,
    );
    assert_eq!(
        run(&mut app, SpatialQueryRequest::new(inverted, asker))
            .items
            .len(),
        3
    );
}

#[test]
fn a_custom_entity_generator_produces_entity_items() {
    let (mut app, asker) = fixture();
    let a = spawn_at(&mut app, Vec3::new(1.0, 0.0, 0.0), Some("crate"));
    let b = spawn_at(&mut app, Vec3::new(2.0, 0.0, 0.0), Some("crate"));
    app.world_mut()
        .resource_mut::<SpatialQueryManager>()
        .register_generator("crates", Box::new(Crates));
    let name = register(
        &mut app,
        r#"(format_version: 2, name: "e", item_type: Entity, default_pick: All,
            generator: Custom(name: "crates"))"#,
    );
    let result = run(&mut app, SpatialQueryRequest::new(name, asker));
    let entities: Vec<_> = result.items.iter().filter_map(|i| i.entity).collect();
    assert_eq!(entities, vec![a, b]);
}

#[test]
fn an_unknown_name_is_reported_as_a_missing_system_naming_it() {
    let (mut app, asker) = fixture();
    let queries = [
        (
            "game_generator",
            r#"(format_version: 2, name: "u1", item_type: Point, generator: Custom(name: "game_generator"))"#,
        ),
        (
            "nested_generator",
            r#"(format_version: 2, name: "u2", item_type: Point, generator: Union(children: [
                Ring(radius: 1.0, count: 1), Custom(name: "nested_generator")]))"#,
        ),
        (
            "game_test",
            r#"(format_version: 2, name: "u3", item_type: Point, generator: Ring(radius: 1.0, count: 1),
                tests: [Custom(id: "x", purpose: Filter, name: "game_test")])"#,
        ),
    ];
    for (unknown, text) in queries {
        let name = register(&mut app, text);
        let result = run(&mut app, SpatialQueryRequest::new(name, asker));
        assert_eq!(result.status, QueryStatus::Failed);
        assert_eq!(result.code, Some("sq_missing_system"));
        assert!(
            result.message.as_deref().is_some_and(|m| m.contains(unknown)),
            "{:?}",
            result.message
        );
    }
}

#[test]
fn a_generator_of_the_wrong_item_type_is_a_mismatch_unless_a_union_allows_it() {
    let (mut app, asker) = fixture();
    let thing = spawn_at(&mut app, Vec3::new(1.0, 0.0, 0.0), None);
    {
        let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
        manager.register_generator("entity", Box::new(Fixed(vec![GeneratedItem::Entity(thing)])));
        manager.register_generator("point", Box::new(Fixed(vec![GeneratedItem::Point(Vec3::ONE)])));
    }
    let entity_in_point = register(
        &mut app,
        r#"(format_version: 2, name: "m1", item_type: Point, generator: Custom(name: "entity"))"#,
    );
    let point_in_entity = register(
        &mut app,
        r#"(format_version: 2, name: "m2", item_type: Entity, generator: Custom(name: "point"))"#,
    );
    for name in [entity_in_point, point_in_entity] {
        let result = run(&mut app, SpatialQueryRequest::new(name, asker));
        assert_eq!(result.status, QueryStatus::Failed);
        assert_eq!(result.code, Some("sq_item_type_mismatch"));
    }
    let mixed = register(
        &mut app,
        r#"(format_version: 2, name: "m3", item_type: Point, default_pick: All,
            generator: Union(allow_mixed: true, children: [Custom(name: "entity")]))"#,
    );
    let result = run(&mut app, SpatialQueryRequest::new(mixed, asker));
    assert_eq!(result.status, QueryStatus::Success);
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].entity, None, "allow_mixed flattens to a position");
}

#[test]
fn bad_output_from_an_implementation_never_panics() {
    let (mut app, asker) = fixture();
    {
        let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
        // A non-finite point is dropped; a despawned entity has no position and is dropped.
        manager.register_generator(
            "nan",
            Box::new(Fixed(vec![
                GeneratedItem::Point(Vec3::NAN),
                GeneratedItem::Point(Vec3::new(1.0, 0.0, 0.0)),
            ])),
        );
        manager.register_generator(
            "many",
            Box::new(Fixed(vec![GeneratedItem::Point(Vec3::ZERO); 21])),
        );
        manager.config.max_items = 20;
        manager.register_test("wrong_kind", Box::new(Returns(TestValue::Bool(true))));
        manager.register_test("nan_value", Box::new(Returns(TestValue::Float(Some(f32::NAN)))));
        manager.register_test("infinite_first", Box::new(InfiniteEast));
    }
    let nan = register(
        &mut app,
        r#"(format_version: 2, name: "n", item_type: Point, default_pick: All, generator: Custom(name: "nan"))"#,
    );
    assert_eq!(run(&mut app, SpatialQueryRequest::new(nan, asker)).items.len(), 1);

    let many = register(
        &mut app,
        r#"(format_version: 2, name: "many", item_type: Point, generator: Custom(name: "many"))"#,
    );
    assert_eq!(
        run(&mut app, SpatialQueryRequest::new(many, asker)).code,
        Some("sq_too_many_items")
    );

    let wrong = register(
        &mut app,
        r#"(format_version: 2, name: "w", item_type: Point, generator: Ring(radius: 1.0, count: 1),
            tests: [Custom(id: "x", purpose: Filter, name: "wrong_kind", value: Float)])"#,
    );
    let result = run(&mut app, SpatialQueryRequest::new(wrong, asker));
    assert_eq!(result.status, QueryStatus::Failed);
    assert_eq!(result.code, Some("sq_bad_param"));

    // Infinity is "no value" too: one such item must not turn every item's score into a NaN.
    let inf = register(
        &mut app,
        r#"(format_version: 2, name: "i", item_type: Point, default_pick: All, generator: Ring(radius: 1.0, count: 4),
            tests: [Custom(id: "x", purpose: Score, name: "infinite_first", value: Float)])"#,
    );
    let result = run(&mut app, SpatialQueryRequest::new(inf, asker));
    assert_eq!(result.status, QueryStatus::Success);
    assert_eq!(result.items.len(), 4);
    assert!(
        result.items.iter().all(|i| i.score.is_finite()),
        "{:?}",
        result.items
    );

    // A NaN is "no value": a score row ranks it worst and the score stays finite.
    let scored = register(
        &mut app,
        r#"(format_version: 2, name: "s", item_type: Point, default_pick: All, generator: Ring(radius: 1.0, count: 2),
            tests: [Custom(id: "x", purpose: Score, name: "nan_value", value: Float)])"#,
    );
    let result = run(&mut app, SpatialQueryRequest::new(scored, asker));
    assert_eq!(result.status, QueryStatus::Success);
    assert!(result.items.iter().all(|i| i.score.is_finite()));
}

#[test]
fn format_v2_loads_v1_files_and_a_v1_file_may_not_use_custom_rows() {
    assert_eq!(SPATIAL_QUERY_FORMAT_VERSION, 2);
    assert_eq!(spatial_query_migration_sources(), vec![1]);

    let v1 = "// keep this comment\n(\n    format_version: 1,\n    name: \"q\",\n    item_type: Point,\n    generator: Ring(radius: 1.0, count: 1),\n)\n";
    let loaded = SpatialQueryAsset::parse(v1).expect("a v1 file loads");
    assert_eq!(loaded.format_version, SPATIAL_QUERY_FORMAT_VERSION);

    let migrated = migrate_ron_v1_to_v2(v1).expect("migrates");
    assert_eq!(migrated, v1.replace("format_version: 1", "format_version: 2"));
    assert_eq!(SpatialQueryAsset::parse(&migrated).expect("loads"), loaded);
    assert_eq!(migrate_ron_v1_to_v2(&migrated).expect("unchanged"), migrated);

    let future = v1.replace("format_version: 1", "format_version: 3");
    assert_eq!(
        migrate_ron_v1_to_v2(&future).expect_err("future").code(),
        "sq_unreadable"
    );
    assert_eq!(
        SpatialQueryAsset::parse(&future).expect_err("future").code(),
        "sq_unreadable"
    );

    // Custom rows arrived with v2.
    let generator = r#"(format_version: 1, name: "q", item_type: Point, generator: Custom(name: "g"))"#;
    let error = SpatialQueryAsset::parse(generator).expect_err("v1 with a custom generator");
    assert_eq!(error.code(), "sq_unreadable");
    assert!(error.to_string().contains("format_version 2"), "{error}");
    let test = r#"(format_version: 1, name: "q", item_type: Point, generator: Ring(radius: 1.0, count: 1),
        tests: [Custom(id: "x", purpose: Filter, name: "t")])"#;
    assert_eq!(
        SpatialQueryAsset::parse(test)
            .expect_err("v1 with a custom test")
            .code(),
        "sq_unreadable"
    );
}

#[test]
fn custom_rows_round_trip_and_bad_rows_report_line_and_column() {
    let asset = SpatialQueryAsset::parse(CUSTOM).expect("loads");
    let text = asset.to_ron().expect("writes");
    assert_eq!(SpatialQueryAsset::parse(&text).expect("reloads"), asset);

    let no_name = "(\n    format_version: 2,\n    name: \"q\",\n    item_type: Point,\n    generator: Custom(name: \"\"),\n)";
    let error = SpatialQueryAsset::parse(no_name).expect_err("empty name");
    assert_eq!(error.code(), "sq_unreadable");
    assert_eq!(error.position(), (5, 16));

    let no_test_name = "(format_version: 2, name: \"q\", item_type: Point, generator: Ring(radius: 1.0, count: 1), tests: [Custom(id: \"x\", purpose: Filter, name: \"\")])";
    assert_eq!(
        SpatialQueryAsset::parse(no_test_name)
            .expect_err("empty name")
            .code(),
        "sq_unreadable"
    );

    let threshold = "(format_version: 2, name: \"q\", item_type: Point, generator: Ring(radius: 1.0, count: 1), tests: [Custom(id: \"x\", purpose: Score, name: \"t\", value: Float, score: (clamp_max: FilterBound))])";
    assert_eq!(
        SpatialQueryAsset::parse(threshold)
            .expect_err("no threshold")
            .code(),
        "sq_no_threshold"
    );

    let candidate_center = "(format_version: 2, name: \"q\", item_type: Point, generator: Custom(name: \"g\", center: Candidate()))";
    assert_eq!(
        SpatialQueryAsset::parse(candidate_center)
            .expect_err("candidate outside a test")
            .code(),
        "sq_unreadable"
    );
}

#[test]
fn the_manager_lists_what_is_registered_sorted() {
    let (app, _) = fixture();
    let manager = app.world().resource::<SpatialQueryManager>();
    assert_eq!(manager.generator_names(), vec!["ring"]);
    assert_eq!(manager.test_names(), vec!["distance", "east_of"]);
}
