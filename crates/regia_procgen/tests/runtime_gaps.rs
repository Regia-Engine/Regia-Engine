// SPDX-License-Identifier: MPL-2.0
//! Procgen runtime gaps (ADR-2421; spec 62 §§ 5.3, 7.1, 7.3, 7.4, 8): settings the generator stored
//! and the runtime ignored. Each test fails on the base it fixes: `editor_only` refusal and the
//! package strip, `OnLoad` runs with the level-load gate and report, `PerTier` bakes, instance-set
//! collision, content-hash staleness of files, `FromShape`/`FromSpline` bounds and a bake directory
//! that follows a level change. Headless, inline evaluation: fixed frame counts, no sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::world::{
    InstanceCollision, ProcBakeTier, ProcBounds, ProcGenLoadFinished, ProcGenLoadGate, ProcGenMode,
    ProcGenPlugin, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
    ProcInstanceCollider, ProcOutput, bake, bake_path, bake_tier_path, generator_bounds, load_bake,
    request_run, run_now, verify,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_render::quality::QualityTier;
use regia_scene::components::EntityGuid;
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

/// A grid of `count` mesh instances, 4 m apart on X, centered on the generator.
fn grid_graph(count: u32, entries: &str) -> String {
    g(
        vec![
            (
                1,
                n(
                    "gen.grid",
                    &[
                        ("count", &format!("({count}.0, 1.0, 1.0)")),
                        ("step_m", "(4.0, 1.0, 1.0)"),
                    ],
                ),
            ),
            (2, n("spawn.mesh", &[("entries", entries)])),
        ],
        &[(1, "out", 2, "in")],
    )
}

fn files() -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert(
        "rocks.pgraph.ron".to_string(),
        grid_graph(3, "[(mesh: \"rock.glb\")]"),
    );
    // The same three points spawn twice: boxes, then a triangle mesh (`spawn.mesh` passes its points on).
    f.insert(
        "solid.pgraph.ron".to_string(),
        g(
            vec![
                (
                    1,
                    n(
                        "gen.grid",
                        &[("count", "(3.0, 1.0, 1.0)"), ("step_m", "(4.0, 1.0, 1.0)")],
                    ),
                ),
                (
                    2,
                    n(
                        "spawn.mesh",
                        &[("entries", "[(mesh: \"cube.glb\", collision: \"Simple\")]")],
                    ),
                ),
                (
                    3,
                    n(
                        "spawn.mesh",
                        &[("entries", "[(mesh: \"cube.glb\", collision: \"Mesh\")]")],
                    ),
                ),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        ),
    );
    f.insert(
        "unknown_mesh.pgraph.ron".to_string(),
        grid_graph(3, "[(mesh: \"nowhere.glb\", collision: \"Simple\")]"),
    );
    // One instance at mobile tiers, three at desktop High (spec 63 § 10 `flow.quality`).
    f.insert(
        "tiers.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[("count", "(1.0, 1.0, 1.0)")])),
                (
                    2,
                    n(
                        "gen.grid",
                        &[("count", "(3.0, 1.0, 1.0)"), ("step_m", "(4.0, 1.0, 1.0)")],
                    ),
                ),
                (3, n("flow.quality", &[])),
                (4, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
            ],
            &[
                (1, "out", 3, "mobile_low"),
                (2, "out", 3, "high"),
                (3, "out", 4, "in"),
            ],
        ),
    );
    f
}

fn app_with(editor: bool, bake_dir: Option<std::path::PathBuf>, files: BTreeMap<String, String>) -> App {
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        editor,
        source: ProcGraphSource::Memory(files),
        bake_dir,
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(AppTypeRegistry::default());
    app
}

fn spawn(app: &mut App, generator: ProcGenerator, guid: u128) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(20.0, 20.0, 20.0),
                },
                ..generator
            },
            EntityGuid(Guid(guid)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

fn state(app: &App, e: Entity) -> (ProcGenRunState, Option<String>, u64) {
    let st = app.world().get::<ProcGenState>(e);
    (
        st.map_or(ProcGenRunState::Idle, |s| s.state),
        st.and_then(|s| s.last_error.as_ref().map(|x| x.1.clone())),
        st.map_or(0, |s| s.evaluations),
    )
}

fn instances(app: &App, e: Entity) -> usize {
    app.world()
        .get::<ProcOutput>(e)
        .map_or(0, |o| o.0.instance_count())
}

// ---------------------------------------------------------------------------------------------
// (1) editor_only
// ---------------------------------------------------------------------------------------------

/// Outside the editor an `editor_only` generator never runs (`procgen_editor_only`) and its bake is
/// not loaded; the editor runs it, and a bake (a tool) still writes it.
#[test]
fn editor_only_generators_are_refused_outside_the_editor() {
    let dir = regia_test_utils::unique_temp_dir("procgen-gaps-editor-only");
    let editor_only = || ProcGenerator {
        editor_only: true,
        ..ProcGenerator::new("rocks.pgraph.ron")
    };
    let mut game = app_with(false, Some(dir.clone()), files());
    let e = spawn(&mut game, editor_only(), 0xe1);
    request_run(game.world_mut(), e);
    game.update();
    game.update();
    let (st, code, evaluations) = state(&game, e);
    assert_eq!(st, ProcGenRunState::Failed);
    assert_eq!(code.as_deref(), Some("procgen_editor_only"));
    assert_eq!(evaluations, 0, "never evaluated");
    assert_eq!(instances(&game, e), 0);
    assert_eq!(run_now(game.world_mut(), e), Some(ProcGenRunState::Failed));

    // The editor runs it; a bake writes it whatever the host.
    let mut editor = app_with(true, Some(dir.clone()), files());
    let e2 = spawn(&mut editor, editor_only(), 0xe1);
    assert_eq!(run_now(editor.world_mut(), e2), Some(ProcGenRunState::Ready));
    assert_eq!(instances(&editor, e2), 3);
    let written = bake(game.world_mut(), e).expect("bake as a tool");
    assert!(written.is_file());

    // A game host does not load the bake.
    let mut later = app_with(false, Some(dir.clone()), files());
    let e3 = spawn(&mut later, editor_only(), 0xe1);
    later.update();
    assert!(
        later.world().get::<ProcOutput>(e3).is_none(),
        "editor-only bake not loaded"
    );
    assert_eq!(
        load_bake(later.world_mut(), e3)
            .map(|_| ())
            .expect_err("refused")
            .0,
        "procgen_editor_only"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The package strip reads the scene format the level saver writes: a snapshot of a world with an
/// `editor_only` generator names it, and stripping removes it with its children.
#[test]
fn the_package_strip_finds_editor_only_generators_in_a_saved_level() {
    use regia_scene::spawn::{SnapshotFilter, snapshot_scene};
    let mut app = app_with(true, None, files());
    {
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        registry.write().register::<ProcGenerator>();
    }
    let kept = spawn(&mut app, ProcGenerator::new("rocks.pgraph.ron"), 0xa1);
    let dropped = spawn(
        &mut app,
        ProcGenerator {
            editor_only: true,
            ..ProcGenerator::new("rocks.pgraph.ron")
        },
        0xb2,
    );
    app.world_mut()
        .spawn((EntityGuid(Guid(0xc3)), Transform::default(), ChildOf(dropped)));
    let _ = kept;
    let mut scene = snapshot_scene(app.world_mut(), &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(
        regia_procgen::cook::editor_only_generators(&scene),
        vec![Guid(0xb2)]
    );
    let text = scene.to_ron_string().expect("ron");
    assert!(regia_procgen::cook::level_text_may_strip(&text));
    let strip = regia_procgen::cook::strip_editor_only(&mut scene);
    assert_eq!(strip.entities, 2, "the generator and its child");
    let left: Vec<u128> = scene.entities.iter().map(|e| e.guid.0).collect();
    assert_eq!(left, vec![0xa1]);
}

// ---------------------------------------------------------------------------------------------
// (2) OnLoad, wait_on_load and the load report
// ---------------------------------------------------------------------------------------------

fn finished(app: &mut App) -> Vec<ProcGenLoadFinished> {
    app.world_mut()
        .resource_mut::<Messages<ProcGenLoadFinished>>()
        .drain()
        .collect()
}

/// An `OnLoad` generator runs once when it spawns with a level; the load gate is pending until it is
/// done and then reports it (with the level of the `LevelLoaded` that came with it).
#[test]
fn on_load_generators_run_at_load_and_the_gate_reports_them() {
    let mut app = app_with(false, None, files());
    let level = regia_scene::level::LevelId(Guid(0x1e7e1));
    let e = spawn(
        &mut app,
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            ..ProcGenerator::new("rocks.pgraph.ron")
        },
        0x0a,
    );
    app.world_mut()
        .write_message(regia_scene::level::LevelLoaded { level, entities: 1 });
    app.update();
    assert!(
        app.world().resource::<ProcGenLoadGate>().pending(),
        "the run is in flight"
    );
    assert!(finished(&mut app).is_empty());
    app.update();
    assert!(!app.world().resource::<ProcGenLoadGate>().pending());
    let done = finished(&mut app);
    assert_eq!(done.len(), 1);
    let report = &done[0].report;
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.level, Some(level));
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].guid, Guid(0x0a));
    assert_eq!(report.entries[0].mode, "on_load");
    assert_eq!(report.entries[0].state, ProcGenRunState::Ready);
    assert_eq!(instances(&app, e), 3);
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(state(&app, e).2, 1, "runs once");
    assert!(finished(&mut app).is_empty(), "the gate closed once");
    assert_eq!(
        app.world().resource::<ProcGenLoadGate>().last.as_ref(),
        Some(report)
    );
}

/// A load that takes longer than the timeout fails visibly: the report says `timed_out` and names the
/// generator with `procgen_on_load_timeout`; the run still publishes when it finishes.
#[test]
fn an_on_load_timeout_fails_the_report_visibly() {
    let mut app = app_with(false, None, files());
    app.world_mut()
        .resource_mut::<ProcGenSettings>()
        .on_load_timeout_frames = 1;
    let e = spawn(
        &mut app,
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            ..ProcGenerator::new("rocks.pgraph.ron")
        },
        0x0b,
    );
    app.update();
    let done = finished(&mut app);
    assert_eq!(done.len(), 1);
    let report = &done[0].report;
    assert!(report.timed_out);
    assert!(!report.ok());
    assert_eq!(report.entries[0].code.as_deref(), Some("procgen_on_load_timeout"));
    app.update();
    assert_eq!(state(&app, e).0, ProcGenRunState::Ready, "late, but published");
    assert_eq!(instances(&app, e), 3);
}

/// A failing `OnLoad` run closes the gate with its code; the editor never auto-runs `OnLoad`.
#[test]
fn a_failed_on_load_run_is_in_the_report_and_the_editor_does_not_auto_run() {
    let mut f = files();
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
    let mut app = app_with(false, None, f.clone());
    spawn(
        &mut app,
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            ..ProcGenerator::new("fails.pgraph.ron")
        },
        0x0c,
    );
    app.update();
    app.update();
    let done = finished(&mut app);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].report.entries[0].state, ProcGenRunState::Failed);
    assert_eq!(done[0].report.entries[0].code.as_deref(), Some("check_failed"));

    let mut editor = app_with(true, None, f);
    let e = spawn(
        &mut editor,
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            ..ProcGenerator::new("rocks.pgraph.ron")
        },
        0x0d,
    );
    editor.update();
    editor.update();
    assert_eq!(state(&editor, e).0, ProcGenRunState::Idle);
    assert!(!editor.world().resource::<ProcGenLoadGate>().pending());
}

// ---------------------------------------------------------------------------------------------
// (3) PerTier bakes
// ---------------------------------------------------------------------------------------------

/// `PerTier` writes one file per `bake_tiers` entry; the loader takes the running tier's file, else
/// the nearest lower, else the nearest higher; verify checks every tier.
#[test]
fn per_tier_bakes_write_each_tier_and_load_the_nearest() {
    let dir = regia_test_utils::unique_temp_dir("procgen-gaps-per-tier");
    let per_tier = || ProcGenerator {
        quality_tier: Some(ProcBakeTier::PerTier),
        ..ProcGenerator::new("tiers.pgraph.ron")
    };
    let mut tool = app_with(true, Some(dir.clone()), files());
    tool.world_mut().resource_mut::<ProcGenSettings>().bake_tiers =
        vec![QualityTier::MobileLow, QualityTier::High];
    let e = spawn(&mut tool, per_tier(), 0x71);
    let guid = Guid(0x71);
    bake(tool.world_mut(), e).expect("bake");
    assert!(bake_tier_path(&dir, guid, QualityTier::MobileLow).is_file());
    assert!(bake_tier_path(&dir, guid, QualityTier::High).is_file());
    assert!(!bake_tier_path(&dir, guid, QualityTier::Ultra).exists());
    assert!(!bake_path(&dir, guid).exists(), "no plain file");
    let v = verify(tool.world_mut(), e).expect("verify");
    assert!(v.ok, "{v:?}");
    assert_eq!(
        v.tiers,
        vec![(QualityTier::MobileLow, true), (QualityTier::High, true)]
    );

    let load_at = |running: QualityTier| {
        let mut app = app_with(false, Some(dir.clone()), files());
        app.world_mut().resource_mut::<ProcGenSettings>().running_tier = running;
        let e = spawn(&mut app, per_tier(), 0x71);
        app.update();
        let tier = app.world().get::<ProcGenState>(e).and_then(|s| s.baked_tier);
        (instances(&app, e), tier, state(&app, e).2)
    };
    assert_eq!(load_at(QualityTier::High), (3, Some(QualityTier::High), 0));
    // Medium has no file: the nearest lower one (MobileLow) wins over the higher (High).
    assert_eq!(load_at(QualityTier::Medium), (1, Some(QualityTier::MobileLow), 0));
    // Ultra: nothing at or below it but High.
    assert_eq!(load_at(QualityTier::Ultra), (3, Some(QualityTier::High), 0));

    // A hand-edited tier fails verify, and names it: one byte of the header's output hash (after
    // magic, version, graph, parameter hashes, seed and input hash: offset 108).
    let path = bake_tier_path(&dir, guid, QualityTier::MobileLow);
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[108] ^= 0xff;
    std::fs::write(&path, &bytes).expect("write");
    let v = verify(tool.world_mut(), e).expect("verify");
    assert!(!v.ok);
    assert_eq!(v.tiers[1], (QualityTier::High, true));
    assert!(!v.tiers[0].1);

    // Back to one tier: the per-tier files go.
    tool.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .quality_tier = None;
    bake(tool.world_mut(), e).expect("plain bake");
    assert!(bake_path(&dir, guid).is_file());
    assert!(!bake_tier_path(&dir, guid, QualityTier::High).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bake_tier_names_parse() {
    use regia_procgen::world::{parse_bake_tiers, tier_fallback};
    assert_eq!(
        parse_bake_tiers(&["high", "MobileLow", "mobile_low"]).expect("tiers"),
        vec![QualityTier::MobileLow, QualityTier::High]
    );
    assert_eq!(parse_bake_tiers::<&str>(&[]).expect("all").len(), 7);
    assert_eq!(
        parse_bake_tiers(&["fancy"]).expect_err("unknown").0,
        "procgen_unknown_tier"
    );
    assert_eq!(
        tier_fallback(QualityTier::Deck),
        vec![
            QualityTier::Deck,
            QualityTier::MobileHigh,
            QualityTier::MobileLow,
            QualityTier::Low,
            QualityTier::Medium,
            QualityTier::High,
            QualityTier::Ultra
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// (4) Instance-set collision
// ---------------------------------------------------------------------------------------------

/// A 2 m cube centered on its origin.
fn cube() -> regia_render::CpuMesh {
    let p = |x: f32, y: f32, z: f32| regia_render::LabVertex {
        position: [x, y, z],
        normal: [0.0, 1.0, 0.0],
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [1.0, 0.0, 0.0, 0.0],
    };
    let mut vertices = Vec::new();
    for z in [-1.0, 1.0] {
        for y in [-1.0, 1.0] {
            for x in [-1.0, 1.0] {
                vertices.push(p(x, y, z));
            }
        }
    }
    let quads = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let indices = quads
        .iter()
        .flat_map(|[a, b, c, d]| [*a, *b, *c, *a, *c, *d])
        .collect();
    regia_render::CpuMesh { vertices, indices }
}

fn colliders(
    app: &mut App,
    generator: Entity,
) -> Vec<(InstanceCollision, usize, regia_physics::ColliderShape)> {
    let mut v: Vec<_> = app
        .world_mut()
        .query::<(&ProcInstanceCollider, &regia_physics::Collider)>()
        .iter(app.world())
        .filter(|(c, _)| c.generator == generator)
        .map(|(c, col)| (c.kind, c.instances, col.shape.clone()))
        .collect();
    v.sort_by_key(|(k, _, _)| *k as u8);
    v
}

/// `collision: Simple` makes one static compound of boxes (the mesh bounds through each instance's
/// transform); `Mesh` one triangle mesh of every instance; clearing the output removes both; physics
/// rays hit them.
#[test]
fn instance_sets_with_collision_get_static_colliders() {
    use regia_physics::ColliderShape;
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
    let mut lib = regia_procgen::draw::ProcMeshLibrary(BTreeMap::new());
    lib.insert("cube.glb", Arc::new(cube()), Arc::new(Default::default()));
    app.insert_resource(lib);
    let e = spawn(&mut app, ProcGenerator::new("solid.pgraph.ron"), 0xc0);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let cols = colliders(&mut app, e);
    assert_eq!(cols.len(), 2, "{cols:?}");
    let (kind, n, shape) = &cols[0];
    assert_eq!((*kind, *n), (InstanceCollision::Simple, 3));
    let ColliderShape::Compound { shapes } = shape else {
        panic!("simple collision is a compound: {shape:?}");
    };
    let mut xs: Vec<f32> = shapes.iter().map(|(t, _)| t.translation.x).collect();
    xs.sort_by(f32::total_cmp);
    assert_eq!(xs, vec![-4.0, 0.0, 4.0]);
    assert!(shapes.iter().all(|(_, s)| matches!(
        s,
        ColliderShape::Box { half_extents } if (*half_extents - Vec3::ONE).abs().max_element() < 1e-6
    )));
    let (kind, n, shape) = &cols[1];
    assert_eq!((*kind, *n), (InstanceCollision::Mesh, 3));
    let ColliderShape::TriMesh { vertices, indices } = shape else {
        panic!("mesh collision is a triangle mesh: {shape:?}");
    };
    assert_eq!((vertices.len(), indices.len()), (24, 36));

    // The colliders reach the physics world: a ray down onto each instance hits its top (y = 1).
    for _ in 0..10 {
        app.update();
    }
    let world = app.world();
    let queries = regia_physics::WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: world.get_resource::<regia_physics::PhysicsLayers>(),
    };
    for x in [-4.0, 0.0, 4.0] {
        let hit = queries
            .raycast(
                Vec3::new(x, 10.0, 0.0),
                Vec3::NEG_Y,
                50.0,
                regia_physics::QueryFilter::new(),
            )
            .expect("a prop is solid");
        assert!((hit.point.y - 1.0).abs() < 1e-3, "{x}: {:?}", hit.point);
    }
    assert!(
        queries
            .raycast(
                Vec3::new(2.0, 10.0, 0.0),
                Vec3::NEG_Y,
                50.0,
                regia_physics::QueryFilter::new()
            )
            .is_none(),
        "nothing between the props"
    );

    regia_procgen::world::clear(app.world_mut(), e);
    assert!(colliders(&mut app, e).is_empty(), "clear removes them");
}

/// Without the mesh in the library the instances still collide, as unit boxes; `None` makes none.
#[test]
fn instance_collision_without_the_mesh_uses_unit_boxes() {
    use regia_physics::ColliderShape;
    let mut app = app_with(false, None, files());
    let e = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xc1);
    run_now(app.world_mut(), e);
    let cols = colliders(&mut app, e);
    assert_eq!(cols.len(), 1);
    let ColliderShape::Compound { shapes } = &cols[0].2 else {
        panic!("compound");
    };
    assert_eq!(shapes.len(), 3);
    assert!(shapes.iter().all(|(_, s)| matches!(
        s,
        ColliderShape::Box { half_extents } if (*half_extents - Vec3::splat(0.5)).abs().max_element() < 1e-6
    )));
    let plain = spawn(&mut app, ProcGenerator::new("rocks.pgraph.ron"), 0xc2);
    run_now(app.world_mut(), plain);
    assert!(colliders(&mut app, plain).is_empty());
}

/// A loaded bake builds the same colliders as a run (props stay solid without running the graph).
#[test]
fn a_loaded_bake_builds_instance_colliders() {
    let dir = regia_test_utils::unique_temp_dir("procgen-gaps-bake-collision");
    let mut tool = app_with(true, Some(dir.clone()), files());
    let e = spawn(&mut tool, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xc3);
    bake(tool.world_mut(), e).expect("bake");
    let mut game = app_with(false, Some(dir.clone()), files());
    let e2 = spawn(&mut game, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xc3);
    game.update();
    assert_eq!(state(&game, e2).2, 0, "loaded, not run");
    let cols = colliders(&mut game, e2);
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].1, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// (5) Content-hash staleness of files
// ---------------------------------------------------------------------------------------------

/// Editing a file a run read (the graph here) marks the generator stale; one with
/// `refresh_on_change` re-runs with the new content. Writing the same bytes again does not.
#[test]
fn a_changed_file_marks_the_generator_stale_by_content() {
    let mut app = app_with(false, None, files());
    app.world_mut()
        .resource_mut::<ProcGenSettings>()
        .file_check_frames = 0;
    let e = spawn(
        &mut app,
        ProcGenerator {
            refresh_on_change: true,
            ..ProcGenerator::new("rocks.pgraph.ron")
        },
        0xf1,
    );
    let quiet = spawn(&mut app, ProcGenerator::new("rocks.pgraph.ron"), 0xf2);
    run_now(app.world_mut(), e);
    run_now(app.world_mut(), quiet);
    assert_eq!(instances(&app, e), 3);
    let hashes = app
        .world()
        .get::<ProcGenState>(e)
        .map(|s| s.file_hashes.clone())
        .unwrap_or_default();
    assert!(
        hashes.get("rocks.pgraph.ron").is_some_and(Option::is_some),
        "{hashes:?}"
    );

    // The same text again: nothing changes.
    let same = files();
    app.world_mut().resource_mut::<ProcGenSettings>().source = ProcGraphSource::Memory(same);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(state(&app, e).2, 1);
    assert!(!app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale));

    // New content: five rocks.
    let mut edited = files();
    edited.insert(
        "rocks.pgraph.ron".to_string(),
        grid_graph(5, "[(mesh: \"rock.glb\")]"),
    );
    app.world_mut().resource_mut::<ProcGenSettings>().source = ProcGraphSource::Memory(edited);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(state(&app, e).2, 2, "re-ran once");
    assert_eq!(instances(&app, e), 5);
    // Without refresh_on_change it is only marked stale (it shares the file the other one reads).
    assert!(app.world().get::<ProcGenState>(quiet).is_some_and(|s| s.stale));
    assert_eq!(state(&app, quiet).2, 1);
}

/// The check runs every `file_check_frames` frames, not every frame.
#[test]
fn file_checks_are_throttled() {
    // The editor checks every generator.
    let mut app = app_with(true, None, files());
    app.world_mut()
        .resource_mut::<ProcGenSettings>()
        .file_check_frames = 4;
    let e = spawn(&mut app, ProcGenerator::new("rocks.pgraph.ron"), 0xf3);
    run_now(app.world_mut(), e);
    let mut edited = files();
    edited.insert(
        "rocks.pgraph.ron".to_string(),
        grid_graph(2, "[(mesh: \"rock.glb\")]"),
    );
    app.world_mut().resource_mut::<ProcGenSettings>().source = ProcGraphSource::Memory(edited);
    let stale = |app: &App| app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale);
    let mut frames = 0;
    while !stale(&app) && frames < 20 {
        app.update();
        frames += 1;
    }
    assert!(stale(&app), "noticed");
    assert!(frames <= 5, "within one check interval: {frames}");
}

// ---------------------------------------------------------------------------------------------
// (6) FromShape / FromSpline bounds
// ---------------------------------------------------------------------------------------------

fn aabb(app: &App, e: Entity) -> ([f32; 3], [f32; 3]) {
    generator_bounds(app.world(), e).world_aabb()
}

fn close(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
}

/// `FromShape` is the entity's own `Shape` in world space; `FromSpline` the box of its own spline
/// plus the margin; without one the run fails with `procgen_bounds_missing`.
#[test]
fn bounds_come_from_the_shape_and_the_spline() {
    use regia_scene::shape::{Shape, ShapeKind};
    use regia_scene::spline::{Spline, SplinePoint};
    let mut app = app_with(false, None, files());
    let shaped = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromShape,
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            Shape::new(ShapeKind::Box {
                half_extents: Vec3::new(10.0, 5.0, 4.0),
            }),
            EntityGuid(Guid(0xb1)),
            Transform::from_translation(Vec3::new(100.0, 0.0, -50.0)),
            GlobalTransform::default(),
        ))
        .id();
    assert!(
        close(aabb(&app, shaped).0, [90.0, -5.0, -54.0]),
        "{:?}",
        aabb(&app, shaped)
    );
    assert!(close(aabb(&app, shaped).1, [110.0, 5.0, -46.0]));

    let point = |x: f32, z: f32| SplinePoint {
        position: Vec3::new(x, 0.0, z),
        ..SplinePoint::default()
    };
    let splined = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromSpline { margin_m: 2.0 },
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            Spline {
                points: vec![point(0.0, 0.0), point(20.0, 0.0)],
                ..Spline::default()
            },
            EntityGuid(Guid(0xb2)),
            Transform::from_translation(Vec3::new(0.0, 3.0, 10.0)),
            GlobalTransform::default(),
        ))
        .id();
    let (lo, hi) = aabb(&app, splined);
    assert!(close(lo, [-2.0, 1.0, 8.0]), "{lo:?}");
    assert!(close(hi, [22.0, 5.0, 12.0]), "{hi:?}");

    // A generator that runs over its shape places only inside it: the grid is 3 wide at 4 m.
    assert_eq!(run_now(app.world_mut(), shaped), Some(ProcGenRunState::Ready));

    let bare = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromShape,
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            EntityGuid(Guid(0xb3)),
            Transform::default(),
        ))
        .id();
    assert_eq!(run_now(app.world_mut(), bare), Some(ProcGenRunState::Failed));
    assert_eq!(state(&app, bare).1.as_deref(), Some("procgen_bounds_missing"));
}

/// Editing the generator's own shape marks it stale (its bounds moved).
#[test]
fn editing_the_own_shape_marks_the_generator_stale() {
    use regia_scene::shape::{Shape, ShapeKind};
    let mut app = app_with(false, None, files());
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::FromShape,
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            Shape::new(ShapeKind::Box {
                half_extents: Vec3::splat(10.0),
            }),
            EntityGuid(Guid(0xb4)),
            Transform::default(),
        ))
        .id();
    run_now(app.world_mut(), e);
    app.update();
    app.update();
    assert!(!app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale));
    if let Some(mut s) = app.world_mut().get_mut::<Shape>(e) {
        s.kind = ShapeKind::Box {
            half_extents: Vec3::splat(30.0),
        };
    }
    app.update();
    assert!(app.world().get::<ProcGenState>(e).is_some_and(|s| s.stale));
}

// ---------------------------------------------------------------------------------------------
// (7) The bake directory follows the level
// ---------------------------------------------------------------------------------------------

/// A generator that found no bake in the first level's directory loads one when the host moves the
/// bake directory to the new level's (`follow_level`), without running the graph.
#[test]
fn a_new_bake_directory_reloads_bakes() {
    let root = regia_test_utils::unique_temp_dir("procgen-gaps-follow-level");
    let level = root.join("maps/cave.level.ron");
    let dir = regia_procgen::world::level_bake_dir(&level);
    let mut tool = app_with(true, Some(dir.clone()), files());
    let e = spawn(&mut tool, ProcGenerator::new("rocks.pgraph.ron"), 0xd1);
    bake(tool.world_mut(), e).expect("bake");

    let mut game = app_with(false, Some(root.join("maps/start.procgen")), files());
    let e2 = spawn(&mut game, ProcGenerator::new("rocks.pgraph.ron"), 0xd1);
    game.update();
    assert!(
        game.world().get::<ProcOutput>(e2).is_none(),
        "no bake in the start level"
    );
    regia_procgen::world::follow_level(game.world_mut(), Some(&level));
    assert_eq!(
        game.world().resource::<ProcGenSettings>().bake_dir.as_deref(),
        Some(dir.as_path())
    );
    game.update();
    assert_eq!(instances(&game, e2), 3);
    assert_eq!(state(&game, e2).2, 0, "loaded, not run");
    let _ = std::fs::remove_dir_all(&root);
}

/// An anchored generator with `wait_on_load` (the default) holds the load gate until no cell around
/// its anchors is queued or running; with `wait_on_load: false` the gate never opens for it.
#[test]
fn anchored_wait_on_load_holds_the_gate_until_its_cells_are_resident() {
    use regia_procgen::tiles::{
        AnchorSettings, AnchorSource, ProcGenAnchor, ProcGenTiles, TileSettings, TileState,
    };
    let mut f = files();
    f.insert(
        "cells.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("in.self", &[])),
                (2, n("sample.ground", &[("spacing_m", "8.0")])),
                (3, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        ),
    );
    let anchored = |wait_on_load: bool| ProcGenerator {
        mode: ProcGenMode::Anchored,
        bounds: ProcBounds::Box {
            half_extents: Vec3::new(256.0, 10.0, 256.0),
        },
        seed: Some(3),
        tiles: Some(TileSettings {
            cell_m: 64.0,
            runtime: Some(AnchorSettings {
                anchors: AnchorSource::Tagged,
                base_reach_m: 80.0,
                max_new_per_frame: Some(2),
                wait_on_load,
                ..AnchorSettings::default()
            }),
            ..TileSettings::default()
        }),
        ..ProcGenerator::new("cells.pgraph.ron")
    };
    let mut app = app_with(false, None, f.clone());
    let e = app
        .world_mut()
        .spawn((anchored(true), EntityGuid(Guid(0xa0)), Transform::default()))
        .id();
    app.world_mut().spawn((
        ProcGenAnchor::default(),
        Transform::from_translation(Vec3::new(10.0, 0.0, 10.0)),
    ));
    app.update();
    assert!(
        app.world().resource::<ProcGenLoadGate>().pending(),
        "cells are queued"
    );
    let mut frames = 1;
    let mut done = Vec::new();
    while done.is_empty() && frames < 200 {
        app.update();
        frames += 1;
        done = finished(&mut app);
    }
    assert_eq!(done.len(), 1, "closed after {frames} frames");
    let report = &done[0].report;
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.entries[0].mode, "anchored");
    assert!(frames > 2, "it waited for cells: {frames}");
    let tiles = app.world().resource::<ProcGenTiles>();
    let g = tiles.generators.get(&e).expect("scheduled");
    assert!(!g.entries.is_empty());
    assert!(
        g.entries
            .values()
            .all(|c| matches!(c.state, TileState::Resident | TileState::Releasing)),
        "every wanted cell is in when the gate closes"
    );

    let mut free = app_with(false, None, f);
    free.world_mut()
        .spawn((anchored(false), EntityGuid(Guid(0xa1)), Transform::default()));
    free.update();
    assert!(!free.world().resource::<ProcGenLoadGate>().pending());
    assert!(finished(&mut free).is_empty());
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 (deep-zai-procgen-runtime findings 3 and 4)
// ---------------------------------------------------------------------------------------------

/// Instance colliders built before the host registered their mesh (unit boxes) are rebuilt with the
/// mesh's bounds and triangles when the mesh library changes, as the draw path's lists are, without
/// running the generator again; an unrelated library change leaves them alone.
#[test]
fn instance_colliders_follow_a_mesh_library_change() {
    use regia_physics::ColliderShape;
    let mut app = app_with(false, None, files());
    let e = spawn(&mut app, ProcGenerator::new("solid.pgraph.ron"), 0xc8);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let evaluations = state(&app, e).2;
    // No library yet: both sets fall back to unit boxes, in one compound.
    let cols = colliders(&mut app, e);
    assert_eq!(cols.len(), 1, "{cols:?}");
    assert_eq!((cols[0].0, cols[0].1), (InstanceCollision::Simple, 6));

    // The host loads the mesh (as the editor's `fill_libraries` does after a draw reports it).
    let mut lib = regia_procgen::draw::ProcMeshLibrary(BTreeMap::new());
    lib.insert("cube.glb", Arc::new(cube()), Arc::new(Default::default()));
    app.insert_resource(lib);
    app.update();
    let cols = colliders(&mut app, e);
    assert_eq!(cols.len(), 2, "{cols:?}");
    let (kind, n, shape) = &cols[0];
    assert_eq!((*kind, *n), (InstanceCollision::Simple, 3));
    let ColliderShape::Compound { shapes } = shape else {
        panic!("simple collision is a compound: {shape:?}");
    };
    assert!(shapes.iter().all(|(_, s)| matches!(
        s,
        ColliderShape::Box { half_extents } if (*half_extents - Vec3::ONE).abs().max_element() < 1e-6
    )));
    let (kind, n, shape) = &cols[1];
    assert_eq!((*kind, *n), (InstanceCollision::Mesh, 3));
    assert!(matches!(shape, ColliderShape::TriMesh { vertices, .. } if vertices.len() == 24));
    assert_eq!(state(&app, e).2, evaluations, "rebuilt without a run");

    // A change to another mesh keeps the built colliders (same entities).
    let before: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<ProcInstanceCollider>>()
        .iter(app.world())
        .collect();
    app.world_mut()
        .resource_mut::<regia_procgen::draw::ProcMeshLibrary>()
        .insert("other.glb", Arc::new(cube()), Arc::new(Default::default()));
    app.update();
    let after: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<ProcInstanceCollider>>()
        .iter(app.world())
        .collect();
    assert_eq!(before, after, "nothing of theirs changed");

    // The library goes away: back to unit boxes.
    app.world_mut()
        .remove_resource::<regia_procgen::draw::ProcMeshLibrary>();
    app.update();
    let cols = colliders(&mut app, e);
    assert_eq!(cols.len(), 1, "{cols:?}");
    assert_eq!((cols[0].0, cols[0].1), (InstanceCollision::Simple, 6));
}

/// The output root carries the generator's `LayerMembership` (spec 62 § 18, ADR-1585): a generator
/// in a parked layer applies its output parked (root, prefabs and colliders), one in a live layer
/// applies it live, and a generator without a membership gives a root without one.
#[test]
fn output_roots_copy_the_generator_layer_membership() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable};
    let mut app = app_with(false, None, files());
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Parked, LayerPeers::default());
    table.insert_layer("rocks", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let member = |layer: &str| LayerMembership {
        layers: vec![layer.to_string()],
    };
    let parked = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd8);
    app.world_mut().entity_mut(parked).insert(member("props"));
    let live = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd9);
    app.world_mut().entity_mut(live).insert(member("rocks"));
    let plain = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xda);
    for e in [parked, live, plain] {
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    }
    let root_of = |app: &mut App, generator: Entity| {
        app.world_mut()
            .query::<(Entity, &ProcOutputRoot)>()
            .iter(app.world())
            .find(|(_, r)| r.generator == generator)
            .map(|(e, _)| e)
            .expect("an output root")
    };
    let root = root_of(&mut app, parked);
    assert_eq!(app.world().get::<LayerMembership>(root), Some(&member("props")));
    assert!(app.world().get::<LayerParked>(root).is_some(), "applied parked");
    let colliders_parked = app
        .world_mut()
        .query::<(&ProcInstanceCollider, Option<&LayerParked>)>()
        .iter(app.world())
        .filter(|(c, _)| c.generator == parked)
        .map(|(_, p)| p.is_some_and(|p| p.inherited))
        .collect::<Vec<_>>();
    assert_eq!(colliders_parked, vec![true], "its collider follows the root");

    let root = root_of(&mut app, live);
    assert_eq!(app.world().get::<LayerMembership>(root), Some(&member("rocks")));
    assert!(app.world().get::<LayerParked>(root).is_none(), "applied live");

    let root = root_of(&mut app, plain);
    assert!(app.world().get::<LayerMembership>(root).is_none());
    assert!(app.world().get::<LayerParked>(root).is_none());
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 layer switches at runtime (rev-claude-61 finding 1): output roots follow their layer
// ---------------------------------------------------------------------------------------------

/// rev-claude-61: a runtime layer switch must reach the output root (book: "Switching the layer
/// live brings them back").
#[test]
fn rev61_switching_the_layer_live_unparks_the_output_root() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{
        LayerChanged, LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable, Side,
        apply_layer_switches,
    };
    let mut app = app_with(false, None, files());
    app.add_message::<LayerChanged>();
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Parked, LayerPeers::default());
    app.insert_resource(table);
    let g = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd8);
    app.world_mut().entity_mut(g).insert(LayerMembership {
        layers: vec!["props".to_string()],
    });
    apply_layer_switches(app.world_mut());
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let root = app
        .world_mut()
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(app.world())
        .find(|(_, r)| r.generator == g)
        .map(|(e, _)| e)
        .expect("an output root");
    assert!(app.world().get::<LayerParked>(root).is_some(), "applied parked");
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("props", LayerState::Live, false, Side::Local)
        .expect("known layer");
    apply_layer_switches(app.world_mut());
    eprintln!(
        "generator parked after switch to Live: {}",
        app.world().get::<LayerParked>(g).is_some()
    );
    assert!(
        app.world().get::<LayerParked>(root).is_none(),
        "the root follows its layer back to Live"
    );
}

/// rev-claude-61: switching a live layer off must park the output root (prefabs, colliders).
#[test]
fn rev61_switching_the_layer_off_parks_the_output_root() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{
        LayerChanged, LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable, Side,
        apply_layer_switches,
    };
    let mut app = app_with(false, None, files());
    app.add_message::<LayerChanged>();
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let g = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd8);
    app.world_mut().entity_mut(g).insert(LayerMembership {
        layers: vec!["props".to_string()],
    });
    apply_layer_switches(app.world_mut());
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let root = app
        .world_mut()
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(app.world())
        .find(|(_, r)| r.generator == g)
        .map(|(e, _)| e)
        .expect("an output root");
    assert!(app.world().get::<LayerParked>(root).is_none(), "applied live");
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("props", LayerState::Parked, false, Side::Local)
        .expect("known layer");
    apply_layer_switches(app.world_mut());
    eprintln!(
        "generator parked after switch to Parked: {}",
        app.world().get::<LayerParked>(g).is_some()
    );
    assert!(
        app.world().get::<LayerParked>(root).is_some(),
        "the root follows its layer to Parked"
    );
}

/// A runtime layer switch reaches what hangs under the output root too: the instance collider is
/// parked with the root when the layer goes off and comes back with it, both ways, any number of
/// times (spec 62 § 18).
#[test]
fn a_layer_switch_parks_and_unparks_the_output_colliders_with_the_root() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{
        LayerChanged, LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable, Side,
        apply_layer_switches,
    };
    let mut app = app_with(false, None, files());
    app.add_message::<LayerChanged>();
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let g = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd8);
    app.world_mut().entity_mut(g).insert(LayerMembership {
        layers: vec!["props".to_string()],
    });
    apply_layer_switches(app.world_mut());
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let root = app
        .world_mut()
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(app.world())
        .find(|(_, r)| r.generator == g)
        .map(|(e, _)| e)
        .expect("an output root");
    let parked = |app: &mut App| {
        let colliders = app
            .world_mut()
            .query::<(&ProcInstanceCollider, Option<&LayerParked>)>()
            .iter(app.world())
            .filter(|(c, _)| c.generator == g)
            .map(|(_, p)| p.is_some())
            .collect::<Vec<_>>();
        (
            app.world().get::<LayerParked>(g).is_some(),
            app.world().get::<LayerParked>(root).is_some(),
            colliders,
        )
    };
    assert_eq!(parked(&mut app), (false, false, vec![false]), "applied live");
    for (state, want) in [
        (LayerState::Parked, true),
        (LayerState::Live, false),
        (LayerState::Absent, true),
        (LayerState::Live, false),
    ] {
        app.world_mut()
            .resource_mut::<LayerTable>()
            .request("props", state, false, Side::Local)
            .expect("known layer");
        apply_layer_switches(app.world_mut());
        assert_eq!(
            parked(&mut app),
            (want, want, vec![want]),
            "after the switch to {state:?}: generator, root, collider"
        );
    }
}

/// Tile cells follow their generator's layer at runtime: a resident cell's output entity (drawn
/// like a generator's output, `extract_proc_instances` skips it while it is parked) is parked when
/// the layer goes off and unparked when it comes back, and a cell that becomes resident while the
/// layer is off arrives parked.
#[test]
fn a_layer_switch_parks_and_unparks_resident_tile_cells() {
    use regia_procgen::tiles::{AnchorSettings, AnchorSource, ProcGenAnchor, ProcTileCell, TileSettings};
    use regia_scene::layers::{
        LayerChanged, LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable, Side,
        apply_layer_switches,
    };
    let mut f = files();
    f.insert(
        "cells.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("in.self", &[])),
                (2, n("sample.ground", &[("spacing_m", "8.0")])),
                (3, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        ),
    );
    let mut app = app_with(false, None, f);
    app.add_message::<LayerChanged>();
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                mode: ProcGenMode::Anchored,
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(256.0, 10.0, 256.0),
                },
                seed: Some(3),
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    runtime: Some(AnchorSettings {
                        anchors: AnchorSource::Tagged,
                        base_reach_m: 40.0,
                        max_new_per_frame: Some(32),
                        ..AnchorSettings::default()
                    }),
                    ..TileSettings::default()
                }),
                ..ProcGenerator::new("cells.pgraph.ron")
            },
            EntityGuid(Guid(0xa0)),
            Transform::default(),
            LayerMembership {
                layers: vec!["props".to_string()],
            },
        ))
        .id();
    let anchor = app
        .world_mut()
        .spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(Vec3::new(10.0, 0.0, 10.0)),
        ))
        .id();
    apply_layer_switches(app.world_mut());
    let cells = |app: &mut App| {
        let mut found = app
            .world_mut()
            .query::<(Entity, &ProcTileCell, Option<&LayerParked>)>()
            .iter(app.world())
            .filter(|(_, c, _)| c.generator == e)
            .map(|(cell, c, parked)| (c.key, cell, parked.is_some()))
            .collect::<Vec<_>>();
        found.sort_by_key(|(key, ..)| *key);
        found
    };
    for _ in 0..50 {
        app.update();
        if !cells(&mut app).is_empty() {
            break;
        }
    }
    let live = cells(&mut app);
    assert!(!live.is_empty(), "a cell became resident");
    assert!(live.iter().all(|(.., parked)| !parked), "applied live: {live:?}");

    let switch = |app: &mut App, state: LayerState| {
        app.world_mut()
            .resource_mut::<LayerTable>()
            .request("props", state, false, Side::Local)
            .expect("known layer");
        apply_layer_switches(app.world_mut());
    };
    switch(&mut app, LayerState::Parked);
    let off = cells(&mut app);
    assert_eq!(off.len(), live.len(), "parking keeps the cells resident");
    assert!(
        off.iter().all(|(.., parked)| *parked),
        "parked with the layer: {off:?}"
    );

    // A cell that streams in while the layer is off arrives parked.
    app.world_mut()
        .entity_mut(anchor)
        .insert(Transform::from_translation(Vec3::new(150.0, 0.0, 150.0)));
    let mut more = off.clone();
    for _ in 0..50 {
        app.update();
        more = cells(&mut app);
        if more.iter().any(|(key, ..)| !off.iter().any(|(k, ..)| k == key)) {
            break;
        }
    }
    assert!(
        more.iter().any(|(key, ..)| !off.iter().any(|(k, ..)| k == key)),
        "a new cell became resident: {more:?}"
    );
    assert!(
        more.iter().all(|(.., parked)| *parked),
        "new cells arrive parked: {more:?}"
    );

    switch(&mut app, LayerState::Live);
    let back = cells(&mut app);
    assert!(!back.is_empty());
    assert!(back.iter().all(|(.., parked)| !parked), "live again: {back:?}");

    // A generator that goes away (an `Absent` switch despawns one with a level template record)
    // takes its resident cells with it.
    app.world_mut().entity_mut(e).despawn();
    app.update();
    assert!(cells(&mut app).is_empty(), "the cells went with their generator");
}

/// A generator that is despawned (as an `Absent` layer switch does to a generator with a level
/// template record, which the switch back respawns and runs again) takes its output root, prefabs
/// and colliders with it, so the respawned generator's output is not doubled by the old one.
#[test]
fn a_despawned_generator_takes_its_output_with_it() {
    use regia_procgen::world::ProcOutputRoot;
    let mut app = app_with(false, None, files());
    let g = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd8);
    let other = spawn(&mut app, ProcGenerator::new("unknown_mesh.pgraph.ron"), 0xd9);
    for e in [g, other] {
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    }
    let outputs = |app: &mut App, generator: Entity| {
        let roots = app
            .world_mut()
            .query::<&ProcOutputRoot>()
            .iter(app.world())
            .filter(|r| r.generator == generator)
            .count();
        let colliders = app
            .world_mut()
            .query::<&ProcInstanceCollider>()
            .iter(app.world())
            .filter(|c| c.generator == generator)
            .count();
        (roots, colliders)
    };
    assert_eq!(outputs(&mut app, g), (1, 1));
    app.world_mut().entity_mut(g).despawn();
    app.update();
    assert_eq!(outputs(&mut app, g), (0, 0), "the output went with its generator");
    assert_eq!(
        outputs(&mut app, other),
        (1, 1),
        "another generator's output stays"
    );
    // Removing the component alone counts as going away too.
    app.world_mut().entity_mut(other).remove::<ProcGenerator>();
    app.update();
    assert_eq!(outputs(&mut app, other), (0, 0));
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 follow-ups (rev-claude-62 notes 1 and 2, the layer-move note)
// ---------------------------------------------------------------------------------------------

/// rev-claude-62 note 2 (v0.5.1): generators without an `EntityGuid` share one root-guid scope
/// (`Guid::NIL`); each gets its own derived root guid, so a guid lookup (and a layer switch of one
/// of them) reaches the right root.
#[test]
fn guid_less_generators_get_their_own_output_root_guids() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{LayerMembership, LayerPeers, LayerState, LayerTable};
    let mut app = app_with(false, None, files());
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let spawn_guid_less = |app: &mut App| {
        app.world_mut()
            .spawn((
                ProcGenerator::new("unknown_mesh.pgraph.ron"),
                LayerMembership {
                    layers: vec!["props".to_string()],
                },
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id()
    };
    let a = spawn_guid_less(&mut app);
    let b = spawn_guid_less(&mut app);
    for e in [a, b] {
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    }
    let root_guid = |app: &mut App, generator: Entity| {
        app.world_mut()
            .query::<(Entity, &ProcOutputRoot, &EntityGuid)>()
            .iter(app.world())
            .find(|(_, r, _)| r.generator == generator)
            .map(|(_, _, g)| g.0)
            .expect("a rooted output")
    };
    let (ga, gb) = (root_guid(&mut app, a), root_guid(&mut app, b));
    assert_ne!(ga, gb, "each guid-less generator's root has its own guid");
}

/// sprint-rev-w9-zai-k finding 1 (v0.5.1): the tiled case of the test above. A tile cell's root
/// guid derives from the cell's scope, which for a guid-less generator is
/// `cell_scope(Guid::NIL, key)` and so never `Guid::NIL` itself: two guid-less tiled generators'
/// roots of the same cell used to share one guid. Covers both copies of the derivation: the
/// apply-time one (the generator is in the layer when its cells become resident) and the late-join
/// one in `sync_output_root_layers` (the generator joins the layer after its cells are resident).
#[test]
fn guid_less_tiled_generators_get_their_own_cell_root_guids() {
    use regia_procgen::tiles::{AnchorSettings, AnchorSource, ProcGenAnchor, ProcTileCell, TileSettings};
    use regia_scene::layers::{LayerMembership, LayerPeers, LayerState, LayerTable, apply_layer_switches};
    for late_join in [false, true] {
        let mut f = files();
        f.insert(
            "cells.pgraph.ron".to_string(),
            g(
                vec![
                    (1, n("in.self", &[])),
                    (2, n("sample.ground", &[("spacing_m", "8.0")])),
                    (3, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
                ],
                &[(1, "out", 2, "region"), (2, "out", 3, "in")],
            ),
        );
        let mut app = app_with(false, None, f);
        let mut table = LayerTable::default();
        table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
        app.insert_resource(table);
        let props = || LayerMembership {
            layers: vec!["props".to_string()],
        };
        let spawn_guid_less = |app: &mut App| {
            let mut e = app.world_mut().spawn((
                ProcGenerator {
                    mode: ProcGenMode::Anchored,
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(256.0, 10.0, 256.0),
                    },
                    seed: Some(3),
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        runtime: Some(AnchorSettings {
                            anchors: AnchorSource::Tagged,
                            base_reach_m: 40.0,
                            max_new_per_frame: Some(32),
                            ..AnchorSettings::default()
                        }),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("cells.pgraph.ron")
                },
                Transform::default(),
                GlobalTransform::default(),
            ));
            if !late_join {
                e.insert(props());
            }
            e.id()
        };
        let a = spawn_guid_less(&mut app);
        let b = spawn_guid_less(&mut app);
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(Vec3::new(10.0, 0.0, 10.0)),
        ));
        apply_layer_switches(app.world_mut());
        let cells = |app: &mut App, generator: Entity| {
            let mut found = app
                .world_mut()
                .query::<(&ProcTileCell, Option<&EntityGuid>)>()
                .iter(app.world())
                .filter(|(c, _)| c.generator == generator)
                .map(|(c, guid)| (c.key, guid.map(|g| g.0)))
                .collect::<Vec<_>>();
            found.sort_by_key(|(key, _)| *key);
            found
        };
        for _ in 0..50 {
            app.update();
            if !cells(&mut app, a).is_empty() && cells(&mut app, a).len() == cells(&mut app, b).len() {
                break;
            }
        }
        if late_join {
            for e in [a, b] {
                app.world_mut().entity_mut(e).insert(props());
            }
            app.update();
        }
        let (ca, cb) = (cells(&mut app, a), cells(&mut app, b));
        assert!(!ca.is_empty(), "late_join {late_join}: cells became resident");
        assert_eq!(
            ca.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            cb.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            "late_join {late_join}: same settings, same anchor, same resident cells"
        );
        let mut guids: Vec<Guid> = Vec::new();
        for (key, guid) in ca.iter().chain(cb.iter()) {
            let Some(guid) = guid else {
                panic!("late_join {late_join}: cell {key:?}'s root is in a layer and has a guid");
            };
            guids.push(*guid);
        }
        let all = guids.len();
        guids.sort();
        guids.dedup();
        assert_eq!(
            guids.len(),
            all,
            "late_join {late_join}: every cell root of the two guid-less generators has its own guid: \
             {ca:?} vs {cb:?}"
        );
    }
}

/// The procgen-runtime-fixes layer-move note (v0.5.1): moving a generator to another layer after
/// its output was applied moves the applied output root too, without waiting for the next apply
/// (the apply used to be the only thing that copied the new membership).
#[test]
fn moving_a_generator_to_another_layer_moves_its_output_root() {
    use regia_procgen::world::ProcOutputRoot;
    use regia_scene::layers::{
        LayerChanged, LayerMembership, LayerParked, LayerPeers, LayerState, LayerTable, Side,
        apply_layer_switches,
    };
    let mut app = app_with(false, None, files());
    app.add_message::<LayerChanged>();
    let mut table = LayerTable::default();
    table.insert_layer("props", None, LayerState::Live, LayerPeers::default());
    table.insert_layer("rocks", None, LayerState::Live, LayerPeers::default());
    app.insert_resource(table);
    let g = spawn(
        &mut app,
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            ..ProcGenerator::new("unknown_mesh.pgraph.ron")
        },
        0xd8,
    );
    app.world_mut().entity_mut(g).insert(LayerMembership {
        layers: vec!["props".to_string()],
    });
    apply_layer_switches(app.world_mut());
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let root = app
        .world_mut()
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(app.world())
        .find(|(_, r)| r.generator == g)
        .map(|(e, _)| e)
        .expect("an output root");
    assert_eq!(
        app.world().get::<LayerMembership>(root).map(|m| m.layers.clone()),
        Some(vec!["props".to_string()]),
        "applied in props"
    );
    // Move the generator to another layer; the applied root follows without a new apply.
    app.world_mut().entity_mut(g).insert(LayerMembership {
        layers: vec!["rocks".to_string()],
    });
    app.update();
    assert_eq!(
        app.world().get::<LayerMembership>(root).map(|m| m.layers.clone()),
        Some(vec!["rocks".to_string()]),
        "the applied root follows the generator's new layer"
    );
    // And the new layer's switches reach it: parking rocks parks the root, parking props does not.
    let switch = |app: &mut App, layer: &str, state: LayerState| {
        app.world_mut()
            .resource_mut::<LayerTable>()
            .request(layer, state, false, Side::Local)
            .expect("known layer");
        apply_layer_switches(app.world_mut());
    };
    switch(&mut app, "props", LayerState::Parked);
    assert!(
        app.world().get::<LayerParked>(root).is_none(),
        "the old layer no longer governs the root"
    );
    switch(&mut app, "rocks", LayerState::Parked);
    assert!(
        app.world().get::<LayerParked>(root).is_some(),
        "the new layer parks the applied root"
    );
}

/// rev-claude-62 note 1 (v0.5.1): the level-template round trip ADR-7556 was written for: a
/// generator with a template record in an `Absent` layer is despawned with its output, and the
/// switch back to `Live` respawns it and applies exactly one new output (one root, one collider
/// set), not a doubled one.
#[test]
fn a_level_template_generator_round_trips_absent_to_live_with_one_output() {
    use regia_ecs::reflect::AppTypeRegistry;
    use regia_ecs::serialize::ReflectCodec;
    use regia_procgen::world::{ProcInstanceCollider, ProcOutputRoot};
    use regia_scene::components::EntityGuid as GuidComponent;
    use regia_scene::layers::{LayerChanged, LayerState, LayerTable, Side, apply_layer_switches};
    use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode};

    let generator_guid = Guid(0x77);
    // Not `app_with`: it replaces the type registry with an empty one, and this test reflects a
    // `ProcGenerator` into a level record and back through the scene loader.
    let mut app = {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            editor: false,
            source: ProcGraphSource::Memory(files()),
            bake_dir: None,
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.add_plugins(regia_scene::ScenePlugin);
        app
    };
    app.add_message::<LayerChanged>();
    // The level's layer definitions (the asset the scene's LevelLayers binding references).
    let defs_id = Guid(0xd0);
    let defs = regia_scene::layers::LayerDefs::from_ron_str(
        r#"LayerDefs(format_version: 1, layers: [
            (name: "props", kind: Runtime, debug_color: (0.2, 0.2, 0.2), parent: None),
        ])"#,
    )
    .expect("defs");
    app.world_mut()
        .resource_mut::<regia_scene::layers::LayerDefsLibrary>()
        .insert(defs_id, defs);
    // The level's scene: one entity record for the generator (guid, OnLoad, membership).
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let generator = ProcGenerator {
        bounds: ProcBounds::Box {
            half_extents: Vec3::new(20.0, 20.0, 20.0),
        },
        mode: ProcGenMode::OnLoad,
        ..ProcGenerator::new("unknown_mesh.pgraph.ron")
    };
    let generator_ron = ReflectCodec::new(&registry.read())
        .to_ron_typed(&generator)
        .expect("generator ron");
    let text = format!(
        r#"(format_version: {}, entities: [
            (guid: "{generator_guid}", components: {{
                "regia_procgen::world::ProcGenerator": {generator_ron},
                "regia_scene::layers::LayerMembership": (layers: ["props"]),
                "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
            }}),
        ], resources: {{ "regia_scene::layers::LevelLayers": (
            defs: ["asset:{defs_id}"],
            entries: [(layer: "props", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone)],
        ) }})"#,
        regia_scene::format::SCENE_FORMAT_VERSION
    );
    let id = LevelId(Guid(0x1e));
    let level = Level::from_ron_str(id, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    regia_scene::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    apply_layer_switches(app.world_mut());

    let outputs = |app: &mut App| {
        let world = app.world_mut();
        let roots = world
            .query::<&ProcOutputRoot>()
            .iter(world)
            .filter(|r| {
                world
                    .get::<GuidComponent>(r.generator)
                    .is_some_and(|g| g.0 == generator_guid)
            })
            .count();
        let colliders = world
            .query::<&ProcInstanceCollider>()
            .iter(world)
            .filter(|c| {
                world
                    .get::<GuidComponent>(c.generator)
                    .is_some_and(|g| g.0 == generator_guid)
            })
            .count();
        (roots, colliders)
    };
    // The OnLoad generator runs at load: one root, one collider set.
    for _ in 0..10 {
        app.update();
        if outputs(&mut app) == (1, 1) {
            break;
        }
    }
    assert_eq!(outputs(&mut app), (1, 1), "the level's generator applied once");

    // Absent: the generator is despawned (it has a template record) and its output goes with it.
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("props", LayerState::Absent, false, Side::Local)
        .expect("known layer");
    apply_layer_switches(app.world_mut());
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(outputs(&mut app), (0, 0), "the absent generator took its output");

    // Live again: the record respawns the generator, which re-applies exactly one output.
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("props", LayerState::Live, false, Side::Local)
        .expect("known layer");
    apply_layer_switches(app.world_mut());
    for _ in 0..10 {
        app.update();
        if outputs(&mut app) == (1, 1) {
            break;
        }
    }
    assert_eq!(
        outputs(&mut app),
        (1, 1),
        "the respawned generator applies exactly one new output"
    );
}
