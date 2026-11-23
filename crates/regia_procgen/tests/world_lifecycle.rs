// SPDX-License-Identifier: MPL-2.0
//! Spec 62 § 10 ECS tests 10-12: lifecycle ordering, ownership of spawned entities, and
//! dependencies (headless minimal app, inline evaluation: fixed frame counts, no sleeps).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::world::{
    ProcBounds, ProcGenLog, ProcGenPlugin, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator,
    ProcGraphSource, ProcOutput, ProcOutputRoot, ProcTerrainEdited, clear, find_generator, request_run,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::{EntityGuid, Generated, Name, Tags};
use regia_scene::spawn::{SnapshotFilter, snapshot_scene};
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

fn g(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> String {
    ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges
            .iter()
            .map(|(a, ap, b, bp)| Edge::new(*a, ap, *b, bp))
            .collect(),
        ..ProcGraph::default()
    }
    .to_ron()
}

fn files() -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    // Three prefab spawns on a grid.
    f.insert(
        "prefabs.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
                (
                    2,
                    n(
                        "spawn.prefab",
                        &[
                            ("prefab", "\"props/crate.prefab.ron\""),
                            ("tags", "[\"spawned\"]"),
                        ],
                    ),
                ),
            ],
            &[(1, "out", 2, "in")],
        ),
    );
    // Three mesh instances and three prefab entities (detach).
    f.insert(
        "both.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
                (2, n("spawn.mesh", &[("entries", "[(mesh: \"m.glb\")]")])),
                (3, n("spawn.prefab", &[("prefab", "\"props/crate.prefab.ron\"")])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        ),
    );
    // A graph that always fails.
    f.insert(
        "fails.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[])),
                (2, n("debug.check", &[("max_count", "1")])),
            ],
            &[(1, "out", 2, "in")],
        ),
    );
    // Reads entities tagged `post` (as points) and exports them.
    f.insert(
        "reads_posts.pgraph.ron".to_string(),
        g(
            vec![
                (
                    1,
                    n("in.entities", &[("tags", "[\"post\"]"), ("within", "world")]),
                ),
                (2, n("out.points", &[])),
            ],
            &[(1, "out", 2, "in")],
        ),
    );
    // Samples the ground.
    f.insert(
        "ground.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("in.self", &[])),
                (2, n("sample.ground", &[("spacing_m", "8.0")])),
                (3, n("out.points", &[])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        ),
    );
    // Generator ordering (test 14): `b` reads `a`'s exported points; `c` and `d` read each other.
    f.insert(
        "exports.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[("count", "(5.0, 1.0, 2.0)")])),
                (2, n("out.points", &[("name", "\"rocks\"")])),
            ],
            &[(1, "out", 2, "in")],
        ),
    );
    let reader = |from: &str| {
        g(
            vec![
                (
                    1,
                    n(
                        "in.generator_output",
                        &[("generator", &format!("\"{from}\"")), ("table", "\"rocks\"")],
                    ),
                ),
                (2, n("out.points", &[("name", "\"rocks\"")])),
            ],
            &[(1, "out", 2, "in")],
        )
    };
    f.insert("reads_a.pgraph.ron".to_string(), reader("a"));
    // Physics rays (spec 63 § 3 `in.rays`).
    f.insert(
        "rays.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("in.self", &[])),
                (
                    2,
                    n("in.rays", &[("spacing_m", "4.0"), ("from_height_m", "50.0")]),
                ),
                (3, n("out.points", &[("name", "\"hits\"")])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        ),
    );
    // Collider overlap (spec 63 § 3 `in.overlap`): a 3 x 1 grid at x = -4, 0, 4; a box sits around x = 4.
    f.insert(
        "overlap.pgraph.ron".to_string(),
        g(
            vec![
                (
                    1,
                    n(
                        "gen.grid",
                        &[("count", "(3.0, 1.0, 1.0)"), ("step_m", "(4.0, 1.0, 1.0)")],
                    ),
                ),
                (2, n("in.overlap", &[])),
                (3, n("out.points", &[("name", "\"touching\"")])),
            ],
            &[(1, "out", 2, "in"), (2, "inside", 3, "in")],
        ),
    );
    // A data table read untyped by the host (spec 63 § 3 `in.table`).
    f.insert(
        "tables/w.table.ron".to_string(),
        "(format_version: 1, row_type: \"game::W\", rows: { \"a\": (n: 1), \"b\": (n: 2) })".to_string(),
    );
    f.insert(
        "table.pgraph.ron".to_string(),
        g(
            vec![
                (
                    1,
                    n(
                        "in.table",
                        &[("asset", "\"tables/w.table.ron\""), ("row", "\"b\"")],
                    ),
                ),
                (2, n("out.table", &[("name", "\"w\"")])),
            ],
            &[(1, "out", 2, "in")],
        ),
    );
    f.insert("reads_c.pgraph.ron".to_string(), reader("c"));
    // Audit M12: the source named through a parameter (`Bind("src")`).
    f.insert("reads_e2.pgraph.ron".to_string(), reader("e2"));
    let mut bound = regia_procgen::ProcGraph::from_ron(&reader("unused")).expect("reader graph");
    if let Some(node) = bound.nodes.get_mut(&1) {
        node.settings.insert(
            "generator".to_string(),
            regia_procgen::SettingValue::parse("Bind(\"src\")").expect("bind"),
        );
    }
    bound.params.push(regia_procgen::graph::ParamDecl {
        name: "src".into(),
        ty: regia_procgen::AttrType::Name,
        default: regia_procgen::SettingValue::Str("nobody".into()),
        min: None,
        max: None,
        doc: String::new(),
    });
    f.insert("reads_bound.pgraph.ron".to_string(), bound.to_ron());
    f.insert("reads_d.pgraph.ron".to_string(), reader("d"));
    f
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(AppTypeRegistry::default());
    app
}

fn generator(app: &mut App, graph: &str, guid: u128) -> Entity {
    let half = if graph == "rays.pgraph.ron" { 8.0 } else { 20.0 };
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(half, 20.0, half),
                },
                ..ProcGenerator::new(graph)
            },
            EntityGuid(Guid(guid)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

fn log(app: &App) -> Vec<(&'static str, u64)> {
    app.world()
        .resource::<ProcGenLog>()
        .0
        .iter()
        .map(|(_, k, r)| (*k, *r))
        .collect()
}

fn state(app: &App, e: Entity) -> ProcGenRunState {
    app.world()
        .get::<ProcGenState>(e)
        .map_or(ProcGenRunState::Idle, |s| s.state)
}

#[test]
fn test_10_lifecycle_events_are_ordered_and_failures_keep_the_old_output() {
    let mut app = app();
    let e = generator(&mut app, "prefabs.pgraph.ron", 1);
    request_run(app.world_mut(), e);
    app.update(); // capture + Started
    assert_eq!(state(&app, e), ProcGenRunState::Running);
    app.update(); // evaluate + apply
    assert_eq!(log(&app), vec![("started", 1), ("finished", 1)]);
    assert_eq!(state(&app, e), ProcGenRunState::Ready);

    // A second request mid-run: Cancelled(2) before Started(3).
    request_run(app.world_mut(), e);
    app.update(); // Started(2)
    request_run(app.world_mut(), e); // cancels 2
    app.update(); // Started(3)
    app.update(); // Finished(3)
    assert_eq!(
        log(&app)[2..],
        [("started", 2), ("cancelled", 2), ("started", 3), ("finished", 3)]
    );

    // A failing graph sends Failed and keeps the previous output.
    let hash = app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    app.world_mut().get_mut::<ProcGenerator>(e).expect("gen").graph = "fails.pgraph.ron".into();
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    assert_eq!(log(&app).last(), Some(&("failed", 4)));
    let st = app.world().get::<ProcGenState>(e).expect("state");
    assert_eq!(st.state, ProcGenRunState::Failed);
    assert_eq!(st.output_hash, hash, "previous output stays");
    assert_eq!(st.last_error.as_ref().map(|x| x.1.as_str()), Some("check_failed"));
    assert!(app.world().get::<ProcOutput>(e).is_some());
}

#[test]
fn test_11_spawned_entities_are_owned_skipped_by_save_and_cleared() {
    let mut app = app();
    let e = generator(&mut app, "prefabs.pgraph.ron", 7);
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let world = app.world_mut();
    let spawned: Vec<(Entity, Generated)> = world
        .query::<(Entity, &Generated)>()
        .iter(world)
        .map(|(x, g)| (x, *g))
        .collect();
    assert_eq!(spawned.len(), 4, "3 prefab entities plus the output root");
    assert!(
        spawned
            .iter()
            .all(|(_, g)| g.generator == Guid(7) && g.run_id == 1)
    );
    let roots: Vec<Entity> = world
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(world)
        .filter(|(_, r)| r.generator == e)
        .map(|(x, _)| x)
        .collect();
    assert_eq!(roots.len(), 1);
    let children = world
        .query::<(&ChildOf, &Tags)>()
        .iter(world)
        .filter(|(c, _)| c.parent() == roots[0])
        .count();
    assert_eq!(children, 3, "prefab entities sit under the output root");
    // Level save skips them: give them guids so the snapshot would otherwise see them.
    let ids: Vec<Entity> = spawned.iter().map(|(x, _)| *x).collect();
    for (k, x) in ids.iter().enumerate() {
        world.entity_mut(*x).insert(EntityGuid(Guid(100 + k as u128)));
    }
    let snap = snapshot_scene(world, &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(
        snap.entities.iter().map(|x| x.guid).collect::<Vec<_>>(),
        vec![Guid(7)],
        "only the generator is saved"
    );

    clear(world, e);
    assert_eq!(
        world.query::<&Generated>().iter(world).count(),
        0,
        "clear removes them"
    );
    assert_eq!(log(&app).last(), Some(&("cleared", 0)));

    // Detach keeps the entities, removes `Generated`, and saves instance sets as components.
    let world = app.world_mut();
    world.get_mut::<ProcGenerator>(e).expect("gen").graph = "both.pgraph.ron".into();
    regia_procgen::world::run_now(world, e);
    let d = regia_procgen::world::detach(world, e).expect("detach");
    assert_eq!(d.entities.len(), 3);
    assert_eq!(d.instance_sets.len(), 1);
    assert_eq!(
        world.query::<&Generated>().iter(world).count(),
        0,
        "nothing is generated any more"
    );
    let set = world
        .get::<regia_procgen::world::InstancedMeshSet>(d.instance_sets[0])
        .expect("set");
    assert_eq!(set.instances.len(), 3);
    assert!(
        world.get::<ProcOutput>(e).is_none(),
        "the generator's output is cleared"
    );
    for x in &d.entities {
        assert!(
            world.get::<EntityGuid>(*x).is_some(),
            "detached entities have guids so the level saves them"
        );
    }
    assert_eq!(log(&app).last(), Some(&("detached", 0)));
}

#[test]
fn test_12_dependencies_mark_stale_only_when_read_inputs_change() {
    let mut app = app();
    let post = app
        .world_mut()
        .spawn((
            EntityGuid(Guid(50)),
            Tags::from_iter_str(["post"]),
            Transform::default(),
        ))
        .id();
    let other = app
        .world_mut()
        .spawn((
            EntityGuid(Guid(51)),
            Tags::from_iter_str(["tree"]),
            Transform::default(),
        ))
        .id();
    let e = generator(&mut app, "reads_posts.pgraph.ron", 2);
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let out = app.world().get::<ProcOutput>(e).expect("output");
    assert_eq!(out.0.points["points"].len(), 1, "read the one post");
    app.update(); // change detection settles
    let stale = |app: &App| app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale);
    assert!(!stale(&app));

    app.world_mut()
        .get_mut::<Transform>(other)
        .expect("tf")
        .translation
        .x = 5.0;
    app.update();
    assert!(!stale(&app), "an unrelated entity moved");

    app.world_mut()
        .get_mut::<Transform>(post)
        .expect("tf")
        .translation
        .x = 5.0;
    app.update();
    assert!(stale(&app), "the read entity moved");

    // Terrain: a generator that samples the ground (no terrain here, so give it one).
    let mut app = app_with_terrain();
    let e = generator(&mut app, "ground.pgraph.ron", 3);
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    app.update();
    let stale = |app: &App| app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale);
    app.world_mut().write_message(ProcTerrainEdited {
        rect: [100.0, 100.0, 110.0, 110.0],
        source: None,
    });
    app.update();
    assert!(!stale(&app), "edit outside the run rect");
    app.world_mut().write_message(ProcTerrainEdited {
        rect: [-5.0, -5.0, 5.0, 5.0],
        source: None,
    });
    app.update();
    assert!(stale(&app), "edit inside the run rect");
}

fn app_with_terrain() -> App {
    let mut app = app();
    let map =
        regia_terrain::TerrainMap::from_heights(65, 65, vec![1.0; 65 * 65], 128.0, 1.0, 0.0).expect("map");
    app.insert_resource(regia_procgen::world::ProcGenTerrain(map));
    app
}

fn named(app: &mut App, graph: &str, guid: u128, name: &str) -> Entity {
    let e = generator(app, graph, guid);
    app.world_mut()
        .entity_mut(e)
        .insert(regia_scene::components::Name::new(name));
    e
}

#[test]
fn test_14_a_reader_waits_for_its_source_and_a_read_cycle_fails() {
    let mut app = app();
    let a = named(&mut app, "exports.pgraph.ron", 10, "a");
    let b = named(&mut app, "reads_a.pgraph.ron", 11, "b");
    // Request the reader first: the source still runs first.
    request_run(app.world_mut(), b);
    request_run(app.world_mut(), a);
    for _ in 0..4 {
        app.update();
    }
    let order: Vec<(Entity, &'static str)> = app
        .world()
        .resource::<ProcGenLog>()
        .0
        .iter()
        .map(|(e, k, _)| (*e, *k))
        .collect();
    assert_eq!(
        order,
        vec![(a, "started"), (a, "finished"), (b, "started"), (b, "finished")]
    );
    let copied = app
        .world()
        .get::<ProcOutput>(b)
        .map(|o| o.0.points["rocks"].len());
    assert_eq!(copied, Some(10), "b read a's 10 points");

    // An identical re-run of a leaves b fresh; a changed output of a makes b stale.
    request_run(app.world_mut(), a);
    app.update();
    app.update();
    app.update();
    assert!(
        !app.world().get::<ProcGenState>(b).is_some_and(|s| s.stale),
        "same output, b stays fresh"
    );
    app.world_mut().get_mut::<ProcGenerator>(a).expect("a").seed = Some(99);
    request_run(app.world_mut(), a);
    app.update();
    app.update();
    app.update();
    assert!(
        app.world().get::<ProcGenState>(b).is_some_and(|s| s.stale),
        "a's new output marks b stale"
    );

    let c = named(&mut app, "reads_d.pgraph.ron", 12, "c");
    let _d = named(&mut app, "reads_c.pgraph.ron", 13, "d");
    request_run(app.world_mut(), c);
    app.update();
    let st = app.world().get::<ProcGenState>(c).expect("state");
    assert_eq!(st.state, ProcGenRunState::Failed);
    assert_eq!(
        st.last_error.as_ref().map(|x| x.1.as_str()),
        Some("generator_cycle")
    );
}

/// A reader requested on its own queues an idle source and does not start until that source is
/// `ready`. A later request does not run the source again.
#[test]
fn a_reader_requested_alone_queues_its_idle_source() {
    let mut app = app();
    let a = named(&mut app, "exports.pgraph.ron", 40, "a");
    let b = named(&mut app, "reads_a.pgraph.ron", 41, "b");
    assert_eq!(state(&app, a), ProcGenRunState::Idle);
    request_run(app.world_mut(), b);
    for _ in 0..6 {
        app.update();
    }
    let order: Vec<(Entity, &'static str)> = app
        .world()
        .resource::<ProcGenLog>()
        .0
        .iter()
        .map(|(e, k, _)| (*e, *k))
        .collect();
    assert_eq!(
        order,
        vec![(a, "started"), (a, "finished"), (b, "started"), (b, "finished")]
    );
    let copied = app
        .world()
        .get::<ProcOutput>(b)
        .map(|o| o.0.points["rocks"].len());
    assert_eq!(copied, Some(10), "b read a's 10 points");
    assert_eq!(state(&app, a), ProcGenRunState::Ready);

    let before = order.len();
    request_run(app.world_mut(), b);
    for _ in 0..4 {
        app.update();
    }
    let order: Vec<(Entity, &'static str)> = app
        .world()
        .resource::<ProcGenLog>()
        .0
        .iter()
        .map(|(e, k, _)| (*e, *k))
        .collect();
    assert_eq!(
        &order[before..],
        &[(b, "started"), (b, "finished")],
        "a ready source is not run again"
    );
}

/// Audit M12: a generator that names its source through a parameter (`Bind`) is ordered and
/// cycle-checked like a literal name: `e2` reads `g2` through its `src` parameter and `g2` reads
/// `e2`, so the run fails `generator_cycle` instead of reading a stale export.
#[test]
fn a_bind_bound_generator_read_is_cycle_checked() {
    let mut app = app();
    let e2 = named(&mut app, "reads_bound.pgraph.ron", 30, "e2");
    app.world_mut()
        .get_mut::<ProcGenerator>(e2)
        .expect("e2")
        .params
        .insert("src".to_string(), "\"g2\"".to_string());
    let _g2 = named(&mut app, "reads_e2.pgraph.ron", 31, "g2");
    request_run(app.world_mut(), e2);
    app.update();
    let st = app.world().get::<ProcGenState>(e2).expect("state");
    assert_eq!(st.state, ProcGenRunState::Failed);
    assert_eq!(
        st.last_error.as_ref().map(|x| x.1.as_str()),
        Some("generator_cycle")
    );
}

#[test]
fn the_host_captures_data_tables_for_in_table() {
    let mut app = app();
    let e = generator(&mut app, "table.pgraph.ron", 20);
    regia_procgen::world::run_now(app.world_mut(), e);
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let t = out.table("w").expect("exported table");
    assert_eq!(t.rows, 1, "row `b` only");
    assert_eq!(
        t.column("n"),
        Some(&regia_procgen::AttrColumn::Int(vec![2].into()))
    );
}

#[test]
fn in_rays_casts_a_grid_against_the_physics_world() {
    use regia_physics::{Collider, ColliderShape, RigidBody};
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, ProcGenPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
        EntityGuid(Guid(77)),
    ));
    for _ in 0..10 {
        app.update();
    }
    // Bounds half 8: cells of 4 m from -8 to 8 → 4 x 4 rays, all over the 20 m plate.
    let e = generator(&mut app, "rays.pgraph.ron", 21);
    regia_procgen::world::run_now(app.world_mut(), e);
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let hits = &out.0.points["hits"];
    assert_eq!(hits.len(), 16);
    assert!(
        hits.position.iter().all(|p| p[1].abs() < 1e-3),
        "hits on the plate top"
    );
    assert_eq!(
        hits.attrs.get("entity").and_then(|c| c.get(0)),
        Some(regia_procgen::AttrValue::Name(Guid(77).to_string()))
    );
}

#[test]
fn in_overlap_sorts_points_by_collider_contact() {
    use regia_physics::{Collider, ColliderShape, RigidBody};
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, ProcGenPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    let collider = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(4.0, 0.0, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(1.0, 1.0, 1.0),
                },
                ..Collider::default()
            },
            EntityGuid(Guid(88)),
        ))
        .id();
    for _ in 0..10 {
        app.update();
    }
    let e = generator(&mut app, "overlap.pgraph.ron", 22);
    regia_procgen::world::run_now(app.world_mut(), e);
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let p = &out.0.points["touching"];
    assert_eq!(p.len(), 1, "only the point at x = 4 touches the box");
    assert_eq!(p.position[0][0], 4.0);
    assert_eq!(
        p.attrs.get("entity").and_then(|c| c.get(0)),
        Some(regia_procgen::AttrValue::Name(Guid(88).to_string()))
    );
    // Audit M3: the overlapped collider is a dependency; moving it marks the generator stale.
    let deps = &app.world().get::<ProcGenState>(e).expect("state").deps;
    assert!(
        deps.contains(&regia_procgen::world::ProcDependency::Entity { guid: Guid(88) }),
        "{deps:?}"
    );
    app.world_mut()
        .get_mut::<Transform>(collider)
        .expect("collider")
        .translation
        .x = 6.0;
    app.update();
    assert!(
        app.world().get::<ProcGenState>(e).expect("state").stale,
        "moving the collider marks the generator stale"
    );
}

/// g1 M7: a name shared by two generators resolves by guid, not by the query's archetype order,
/// which adding an unrelated component changes.
#[test]
fn a_shared_generator_name_resolves_by_guid_in_any_archetype_order() {
    #[derive(Component)]
    struct Unrelated;
    let spawn = |world: &mut World, g: u128| {
        world
            .spawn((ProcGenerator::default(), EntityGuid(Guid(g)), Name::new("trees")))
            .id()
    };
    for moved_low in [false, true] {
        let mut world = World::new();
        let high = spawn(&mut world, 9);
        let low = spawn(&mut world, 5);
        let moved = if moved_low { low } else { high };
        world.entity_mut(moved).insert(Unrelated);
        assert_eq!(
            find_generator(&mut world, "trees"),
            Some(low),
            "moved_low {moved_low}"
        );
        assert_eq!(find_generator(&mut world, &Guid(9).to_string()), Some(high));
    }
}

fn second_idle_scans(n: u32) -> u64 {
    let mut app = app();
    for i in 0..n {
        app.world_mut()
            .spawn((EntityGuid(Guid(u128::from(i) + 1)), Tags::from_iter_str(["prop"])));
    }
    app.update();
    regia_procgen::probe::arm();
    app.update();
    regia_procgen::probe::idle_scans()
}

/// An idle world must not walk every saved entity on later frames (audit finding 1).
///
/// A counting allocator is not available here: this crate denies `unsafe` code, so the
/// measurement is the entity-scan counter rather than a byte slope.
#[test]
fn idle_update_does_not_walk_every_entity() {
    let scans = second_idle_scans(4000);
    assert_eq!(scans, 0, "second idle update walked {scans} tagged entities");
}

/// A queued reader resolves `in.generator_output` once, and a vanished graph does not release it.
#[test]
fn queued_generator_reads_its_graph_once() {
    let mut app = app();
    let src = named(&mut app, "exports.pgraph.ron", 10, "a");
    app.world_mut().entity_mut(src).insert(ProcGenState::default());
    {
        let mut st = app
            .world_mut()
            .get_mut::<ProcGenState>(src)
            .expect("source state");
        st.state = ProcGenRunState::Running;
        st.run_id = 1;
        st.on_load_requested = true;
    }
    let reader = named(&mut app, "reads_a.pgraph.ron", 11, "b");
    request_run(app.world_mut(), reader);
    app.update();
    assert_eq!(
        state(&app, reader),
        ProcGenRunState::Queued,
        "the reader waits while its source is in flight"
    );
    regia_procgen::probe::arm();
    app.update();
    assert_eq!(
        regia_procgen::probe::graph_reads(),
        0,
        "the second frame re-read the queued reader's graph"
    );
    assert_eq!(state(&app, reader), ProcGenRunState::Queued);
    {
        let mut settings = app.world_mut().resource_mut::<ProcGenSettings>();
        match &mut settings.source {
            ProcGraphSource::Memory(files) => {
                files.remove("reads_a.pgraph.ron");
            }
            ProcGraphSource::Dir(_) => panic!("the test source is memory"),
        }
    }
    regia_procgen::probe::arm();
    app.update();
    assert_eq!(
        regia_procgen::probe::graph_reads(),
        0,
        "a missing graph was read again instead of keeping the cached names"
    );
    assert_eq!(
        state(&app, reader),
        ProcGenRunState::Queued,
        "a failed re-read must not start the waiter"
    );
}

/// A generator whose graph changes to one that reads another generator waits for that source on the
/// next request, not only after the next file check (corr-procgen: the cached names belonged to the
/// old graph for up to `file_check_frames` frames, so the reader started while its source ran).
#[test]
fn a_changed_graph_waits_for_its_new_source_at_once() {
    let mut app = app();
    let reader = named(&mut app, "exports.pgraph.ron", 11, "b");
    request_run(app.world_mut(), reader);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(state(&app, reader), ProcGenRunState::Ready, "the first graph ran");
    let src = named(&mut app, "exports.pgraph.ron", 10, "a");
    app.world_mut().entity_mut(src).insert(ProcGenState::default());
    {
        let mut st = app
            .world_mut()
            .get_mut::<ProcGenState>(src)
            .expect("source state");
        st.state = ProcGenRunState::Running;
        st.run_id = 1;
        st.on_load_requested = true;
    }
    app.world_mut()
        .get_mut::<ProcGenerator>(reader)
        .expect("reader")
        .graph = "reads_a.pgraph.ron".to_string();
    request_run(app.world_mut(), reader);
    app.update();
    assert_eq!(
        state(&app, reader),
        ProcGenRunState::Queued,
        "the reader's new graph reads `a`, which is still running"
    );
}

/// `procgen.profile` runs a `Baked` generator at its fixed tier, the tier its real run uses
/// (corr-procgen: it ran at the running tier, so a tiered graph profiled other output).
#[test]
fn profile_runs_a_baked_generator_at_its_baked_tier() {
    use regia_procgen::world::{ProcBakeTier, ProcGenMode};
    let mut app = app();
    let tiered = g(
        vec![
            (1, n("gen.grid", &[("count", "(2.0, 1.0, 1.0)")])),
            (2, n("gen.grid", &[("count", "(5.0, 1.0, 1.0)")])),
            (3, n("flow.quality", &[])),
            (4, n("out.points", &[("name", "\"p\"")])),
        ],
        &[(1, "out", 3, "low"), (2, "out", 3, "high"), (3, "out", 4, "in")],
    );
    match &mut app.world_mut().resource_mut::<ProcGenSettings>().source {
        ProcGraphSource::Memory(files) => {
            files.insert("tiered.pgraph.ron".to_string(), tiered);
        }
        ProcGraphSource::Dir(_) => panic!("the test source is memory"),
    }
    let e = generator(&mut app, "tiered.pgraph.ron", 30);
    {
        let mut gen_c = app.world_mut().get_mut::<ProcGenerator>(e).expect("generator");
        gen_c.mode = ProcGenMode::Baked;
        gen_c.quality_tier = Some(ProcBakeTier::Low);
    }
    let runs = regia_procgen::profile::profile_generator(app.world_mut(), e, &[0, 2]).expect("profile");
    for run in &runs {
        let out = run
            .nodes
            .iter()
            .find(|line| line.ty == "flow.quality")
            .expect("the flow.quality line");
        assert_eq!(
            out.elements, 2,
            "threads {}: the Low pin's two points",
            run.threads
        );
    }
}

/// Removing the read tag from an entity marks a generator that queried that tag stale, though no
/// `Tags` changed and the entity count stayed the same (corr-procgen: the cached tag list was
/// reused and the generator never went stale).
#[test]
fn removing_a_queried_tag_marks_the_reader_stale() {
    let mut app = app();
    let post = app
        .world_mut()
        .spawn((
            EntityGuid(Guid(50)),
            Tags::from_iter_str(["post"]),
            Transform::default(),
        ))
        .id();
    let e = generator(&mut app, "reads_posts.pgraph.ron", 2);
    request_run(app.world_mut(), e);
    app.update();
    app.update();
    let out = app.world().get::<ProcOutput>(e).expect("output");
    assert_eq!(out.0.points["points"].len(), 1, "read the one post");
    app.update(); // change detection settles
    app.update();
    let stale = |app: &App| app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale);
    assert!(!stale(&app));
    app.world_mut().entity_mut(post).remove::<Tags>();
    app.update();
    assert!(stale(&app), "the queried tag is gone from the level");
}
