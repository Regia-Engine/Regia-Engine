// SPDX-License-Identifier: MPL-2.0
//! Microbenchmarks for spec 33 § 6, spec 38 § 9 and spec 39 § 12 (`docs/06-testing-and-ci.md` § 9). Every
//! timed test is `#[ignore]`: the gate never runs them, and they assert only that the work happened,
//! never a time. The one test the gate does run counts the work of the spatial-query fixtures, so a
//! number measured by hand is known to have measured the workload the spec names. Run with
//!
//! ```text
//! cargo test -p regia_ai --release --test ai_perf -- --ignored --nocapture
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::smart_object_v2::{QueryRegion, SlotQuery};
use regia_ai::state_tree::{StateTree, StateTreeDef};
use regia_ai::{AiPlugin, BtTaskRegistry, SmartObject, SmartObjectDef, SmartObjectDefinition};
use regia_ai::{SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::{GlobalTransform, Transform};

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

const TREE: &str = r#"(
    format_version: 2,
    root: "Root",
    states: [
        (name: "Root", children: ["Idle", "Alert", "Chase", "Search", "Flee"]),
        (name: "Idle", transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 12.0), target: State("Alert")),
        ]),
        (name: "Alert", transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Gt, value: 20.0), target: State("Idle")),
        ]),
        (name: "Chase"),
        (name: "Search"),
        (name: "Flee"),
    ],
)"#;

/// Spec 38 § 9: under 1 ms per fixed step on one desktop core.
#[test]
#[ignore = "hand-run microbenchmark"]
fn state_trees_1000_of_6_states() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    app.update();
    let def = StateTreeDef::from_ron(TREE).expect("definition");
    for _ in 0..1_000 {
        let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree");
        let mut board = Blackboard::new();
        board.insert("threat".to_owned(), BbValue::Float(100.0));
        app.world_mut().spawn((tree, board));
    }
    for _ in 0..50 {
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().clear_trackers();
    }
    let samples = (0..500)
        .map(|_| {
            let start = Instant::now();
            app.world_mut().run_schedule(FixedUpdate);
            let spent = start.elapsed();
            app.world_mut().clear_trackers();
            spent
        })
        .collect();
    report("state_trees_1000_tick", samples);
}

/// 10,000 slots on a 1 m lattice under one object.
fn slot_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    let object = app.world_mut().spawn(SmartObject::default()).id();
    for x in 0..100 {
        for z in 0..100 {
            let position = Vec3::new(x as f32, 0.0, z as f32);
            app.world_mut().spawn((
                SmartObjectSlot::new("seat"),
                Transform::from_translation(position),
                GlobalTransform(Affine3A::from_translation(position)),
                regia_ecs::prelude::ChildOf(object),
            ));
        }
    }
    app.update();
    app.update();
    app
}

/// Spec 39 § 12: a no-change rebuild of 10,000 slots, informational, under 0.05 ms.
#[test]
#[ignore = "hand-run microbenchmark"]
fn index_rebuild_10000_unchanged() {
    let mut app = slot_app();
    let samples = (0..300)
        .map(|_| {
            let start = Instant::now();
            app.world_mut().run_schedule(PostUpdate);
            let spent = start.elapsed();
            app.world_mut().clear_trackers();
            spent
        })
        .collect();
    report("smart_object_postupdate_10000_unchanged", samples);
}

/// Spec 39 § 12: a full rebuild after every slot moved, informational, under 2 ms.
#[test]
#[ignore = "hand-run microbenchmark"]
fn index_rebuild_10000_all_moved() {
    let mut app = slot_app();
    let mut moved = app.world_mut().query::<&mut GlobalTransform>();
    let samples = (0..200)
        .map(|frame| {
            let offset = Vec3::splat((frame % 2) as f32 * 3.0);
            for mut transform in moved.iter_mut(app.world_mut()) {
                transform.0.translation += regia_core::math::Vec3A::from(offset);
            }
            let start = Instant::now();
            app.world_mut().run_schedule(PostUpdate);
            let spent = start.elapsed();
            app.world_mut().clear_trackers();
            spent
        })
        .collect();
    assert!(app.world().resource::<SmartObjectIndex>().rebuilds() >= 200);
    report("smart_object_postupdate_10000_all_moved", samples);
}

/// Spec 39 § 12: one search of 10,000 slots hitting about 20, informational, under 0.2 ms.
#[test]
#[ignore = "hand-run microbenchmark"]
fn search_10000_slots_about_20_hits() {
    let mut app = slot_app();
    let index = app
        .world_mut()
        .remove_resource::<SmartObjectIndex>()
        .expect("index");
    let world = app.world();
    let query = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::new(50.0, 0.0, 50.0),
        radius: 2.5,
    });
    let samples = (0..2_000)
        .map(|_| {
            let start = Instant::now();
            let hits = index.search(world, black_box(&query), None);
            let spent = start.elapsed();
            assert_eq!(hits.len(), 21);
            spent
        })
        .collect();
    report("smart_object_search_10000", samples);
}

/// Spec 39 § 12: materializing 1,000 two-slot objects, informational, under 8 ms.
#[test]
#[ignore = "hand-run microbenchmark"]
fn materialize_1000_two_slot_objects() {
    let def = std::sync::Arc::new(
        SmartObjectDef::from_ron(r#"(format_version: 1, slots: [(name: "a"), (name: "b")])"#)
            .expect("definition"),
    );
    let samples = (0..20)
        .map(|_| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, SmartObjectPlugin));
            app.update();
            for _ in 0..1_000 {
                app.world_mut()
                    .spawn((SmartObject::default(), SmartObjectDefinition(def.clone())));
            }
            // Timed from here: the first `PostUpdate` that sees the objects does the work.
            let start = Instant::now();
            app.world_mut().run_schedule(PostUpdate);
            start.elapsed()
        })
        .collect();
    report("smart_object_materialize_1000", samples);
}

/// Perception at crowd scale (ADR-2565): 1,000 agents and 1,000 targets on a 256-check budget.
#[test]
#[ignore = "hand-run microbenchmark"]
fn perception_1000_agents_1000_targets_budget_256() {
    use regia_ai::perception_system::{Perceivable, PerceptionPlugin, PerceptionSettings, PerceptionStats};
    use regia_ai::{HearingEvent, Perception};
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(PerceptionPlugin);
    app.update();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 256;
    }
    for i in 0..1_000 {
        let x = (i % 32) as f32 * 2.0;
        let z = (i / 32) as f32 * 2.0;
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(x, 0.0, z)),
            Perception::default(),
            Blackboard::new(),
        ));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(x, 0.0, z - 5.0)),
            Perceivable {
                team: 1,
                ..Perceivable::default()
            },
        ));
    }
    for _ in 0..50 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let samples: Vec<Duration> = (0..500)
        .map(|tick| {
            if tick % 10 == 0 {
                app.world_mut().write_message(HearingEvent {
                    position: Vec3::new(20.0, 0.0, 20.0),
                    radius: 30.0,
                    team: 1,
                    source: Entity::PLACEHOLDER,
                });
            }
            let start = Instant::now();
            app.world_mut().run_schedule(FixedUpdate);
            let spent = start.elapsed();
            black_box(app.world().resource::<PerceptionStats>().tick);
            spent
        })
        .collect();
    report("perception_tick 1000x1000 budget 256", samples);
    assert_eq!(
        app.world().resource::<PerceptionStats>().sight_checks_last_tick,
        256
    );
}

/// Utility scoring at crowd scale (ADR-2565): 1,000 reasoners of 8 actions x 3 considerations, a
/// decision every step for each.
#[test]
#[ignore = "hand-run microbenchmark"]
fn utility_1000_reasoners_every_step() {
    use regia_ai::utility::{ActionDef, ConsiderationDef, ResponseCurve, UtilityAiDef};
    use regia_ai::utility_ecs::{UtilityClock, UtilityPlugin, UtilityReasoner};
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(UtilityPlugin);
    app.update();
    let def = UtilityAiDef {
        actions: (0..8)
            .map(|a| ActionDef {
                name: format!("action{a}"),
                considerations: (0..3)
                    .map(|c| ConsiderationDef {
                        key: format!("key{c}"),
                        min: 0.0,
                        max: 100.0,
                        curve: ResponseCurve::default(),
                    })
                    .collect(),
                weight: 1.0 + a as f32 * 0.1,
                cooldown: 0.0,
                momentum: 0.1,
            })
            .collect(),
        ..UtilityAiDef::default()
    };
    for _ in 0..1_000 {
        let mut reasoner = UtilityReasoner::from_def(&def).expect("valid");
        reasoner.interval_ticks = 1;
        let mut board = Blackboard::new();
        for c in 0..3 {
            board.insert(format!("key{c}"), 50.0f32);
        }
        app.world_mut().spawn((reasoner, board));
    }
    let samples: Vec<Duration> = (0..500)
        .map(|_| {
            let start = Instant::now();
            app.world_mut().run_schedule(FixedUpdate);
            start.elapsed()
        })
        .collect();
    report("utility_tick 1000 reasoners", samples);
    assert_eq!(app.world().resource::<UtilityClock>().decisions_last_tick, 1_000);
}

/// GOAP at crowd scale (ADR-2565): 100 agents replanning every other step over one shared planner of
/// 4 actions, the cache off so each search runs.
#[test]
#[ignore = "hand-run microbenchmark"]
fn goap_100_agents_replanning() {
    use std::sync::Arc;

    use regia_ai::goap::{Action, FactTable, Goal, Planner};
    use regia_ai::goap_ecs::{GoapAgent, GoapPlugin, GoapSettings, GoapStats};
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(GoapPlugin);
    app.update();
    app.world_mut().resource_mut::<GoapSettings>().max_plans_per_tick = 100;
    let planner = Arc::new(Planner::new(vec![
        Action::new("pick_up_axe", 1.0).requires(3, true).sets(0, true),
        Action::new("chop", 1.0).requires(0, true).sets(1, true),
        Action::new("gather_sticks", 8.0).sets(1, true),
        Action::new("craft", 1.0).requires(1, true).sets(2, true),
    ]));
    let agents: Vec<Entity> = (0..100)
        .map(|_| {
            let mut facts = FactTable::new();
            for name in ["has_axe", "has_wood", "has_table", "axe_available"] {
                facts.add_bool(name).expect("fact");
            }
            let mut agent = GoapAgent::new(Arc::clone(&planner), facts, Goal::new().require(2, true));
            agent.options.use_cache = false;
            app.world_mut().spawn((agent, Blackboard::new())).id()
        })
        .collect();
    let samples: Vec<Duration> = (0..200u32)
        .map(|tick| {
            for &agent in &agents {
                let mut board = app.world_mut().get_mut::<Blackboard>(agent).expect("board");
                board.insert("axe_available", tick % 4 < 2);
            }
            let start = Instant::now();
            app.world_mut().run_schedule(FixedUpdate);
            start.elapsed()
        })
        .collect();
    report("goap_tick 100 agents replanning", samples);
    assert!(app.world().resource::<GoapStats>().plans_started_total > 1_000);
}

const LATTICE_QUERY: &str = r#"(
    format_version: 2,
    name: "lattice_441",
    item_type: Point,
    generator: Lattice(center: Asker, half_size: 10.0, spacing: 1.0),
    tests: [
        Cast(id: "clear", purpose: Filter, from: Candidate(height: 1.6), to: Entity(tag: "player", height: 1.6),
             shape: Line, want_hit: false),
        Distance(id: "near", purpose: Score, to: Entity(tag: "player"), mode: Planar, score: (weight: -1.0)),
    ],
)"#;

const RING_QUERY: &str = r#"(
    format_version: 2,
    name: "ring_20000",
    item_type: Point,
    generator: Ring(center: Asker, radius: 100.0, count: 20000),
    tests: [
        Distance(id: "near", purpose: Score, to: Entity(tag: "player"), score: (weight: -1.0)),
    ],
)"#;

/// Spec 33 § 6's workload: an asker, a player 12 m away and four static boxes the casts can hit.
fn spatial_query_app() -> (App, Entity) {
    use regia_ai::spatial_query::{SpatialQueryManager, SpatialQueryPlugin};
    use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
    use regia_scene::components::Tags;

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, SpatialQueryPlugin));
    enable_fixed_step(&mut app, 60.0);
    let place = |p: Vec3| {
        (
            Transform::from_translation(p),
            GlobalTransform(Affine3A::from_translation(p)),
        )
    };
    let asker = app.world_mut().spawn(place(Vec3::ZERO)).id();
    app.world_mut()
        .spawn((place(Vec3::new(12.0, 0.0, 0.0)), Tags::from_iter_str(["player"])));
    for (x, z) in [(6.0, 0.0), (6.0, 5.0), (6.0, -5.0), (-4.0, 4.0)] {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(x, 1.0, z)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.5, 2.0, 1.5),
                },
                ..Collider::default()
            },
        ));
    }
    app.update();
    app.world_mut().run_schedule(FixedUpdate);
    let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
    manager
        .register_template_text(LATTICE_QUERY)
        .expect("the lattice query loads");
    manager
        .register_template_text(RING_QUERY)
        .expect("the ring query loads");
    (app, asker)
}

fn run_query(app: &mut App, name: &str, asker: Entity) -> regia_ai::SpatialQueryResult {
    use regia_ai::spatial_query::{SpatialQueryManager, SpatialQueryRequest};
    app.world_mut()
        .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
            manager.run_instant(world, SpatialQueryRequest::new(name, asker))
        })
}

/// What the timed tests below measure, counted: not ignored, so the gate keeps the workload honest.
#[test]
fn spatial_query_fixtures_do_the_work_the_spec_names() {
    use regia_ai::spatial_query::QueryStatus;
    let (mut app, asker) = spatial_query_app();

    let lattice = run_query(&mut app, "lattice_441", asker);
    assert_eq!(lattice.status, QueryStatus::Success, "{:?}", lattice.message);
    assert_eq!(lattice.tests[0].input, 441, "21 x 21 items reach the cast");
    let survivors = lattice.tests[0].output;
    assert!(
        survivors > 0 && survivors < 441,
        "the walls block some casts: {survivors}"
    );
    assert_eq!(lattice.tests[1].input, survivors);
    // One unit per generated item, one per (item, test, anchor value).
    assert_eq!(lattice.units, 441 + 441 + survivors as u64);

    let ring = run_query(&mut app, "ring_20000", asker);
    assert_eq!(ring.status, QueryStatus::Success, "{:?}", ring.message);
    assert_eq!(ring.item_count, 20_000);
    assert_eq!(ring.units, 20_000 + 20_000);
    assert!(ring.best().is_some());
}

/// Spec 33 § 6: a 21 x 21 lattice with one cast and one distance test, at most 0.6 ms (desktop, release).
#[test]
#[ignore = "hand-run microbenchmark"]
fn spatial_query_lattice_441_cast_and_distance() {
    let (mut app, asker) = spatial_query_app();
    let samples = (0..500)
        .map(|_| {
            let start = Instant::now();
            let result = run_query(&mut app, "lattice_441", asker);
            let spent = start.elapsed();
            assert!(result.best().is_some());
            black_box(result);
            spent
        })
        .collect();
    report("spatial_query_lattice_441", samples);
}

/// Spec 33 § 6: `run_instant` of 20,000 distance-only items, at most 5 ms (desktop, release).
#[test]
#[ignore = "hand-run microbenchmark"]
fn spatial_query_20000_distance_only() {
    let (mut app, asker) = spatial_query_app();
    let samples = (0..200)
        .map(|_| {
            let start = Instant::now();
            let result = run_query(&mut app, "ring_20000", asker);
            let spent = start.elapsed();
            assert_eq!(result.item_count, 20_000);
            black_box(result);
            spent
        })
        .collect();
    report("spatial_query_20000_distance_only", samples);
}
