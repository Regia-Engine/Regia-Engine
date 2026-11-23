// SPDX-License-Identifier: MPL-2.0
//! The world side of generated meshes and terrain edits (spec 64 §§ 6, 7.3, 9 test 13, spec 66 § 2):
//! a generator's terrain edit applied, restored and baked, generated meshes through `.pgout`, and
//! both cut per cell in tiled generators. No GPU.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{graph, node};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::ProcGenPlugin;
use regia_procgen::road::RoadOf;
use regia_procgen::tiles::{TileKey, TileSettings};
use regia_procgen::world::{
    ProcBounds, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource, ProcOutput,
    ProcTerrainEdited, bake, bake_cells, bake_path, clear, detach, load_bake, request_run, run_now, verify,
};
use regia_scene::components::{EntityGuid, Tags};
use regia_scene::spline::{Spline, TangentMode};
use regia_scene::{GlobalTransform, Transform};

const PROFILE: &str = "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, \
    cross_slope_pct: 2.0, samples_per_m: 1.0, material: \"road.material.ron\", \
    markings: [(pattern: \"solid\", offset_m: 0.0, width_m: 0.2, color: (1.0, 0.8, 0.1))], \
    terrain: (flatten: true, flatten_extra_m: 1.0, falloff_m: 4.0, carve_max_m: 3.0, fill_max_m: 3.0))";

/// The road network graph: entities tagged `road`, flatten (when asked), ribbon, markings.
fn road_graph(flatten: bool) -> String {
    let mut nodes = vec![
        (
            1,
            node(
                "in.entities",
                &[
                    ("tags", "[\"road\"]"),
                    ("as", "\"splines\""),
                    ("within", "\"world\""),
                ],
            ),
        ),
        (2, node("in.ground", &[])),
        (3, node("road.ribbon", &[("profile", PROFILE)])),
        (4, node("road.markings", &[("profile", PROFILE)])),
    ];
    let mut edges = vec![
        (2, "out", 3, "ground"),
        (2, "out", 4, "ground"),
        (1, "out", 4, "splines"),
    ];
    if flatten {
        nodes.push((5, node("terrain.flatten_road", &[("profile", PROFILE)])));
        edges.extend([
            (1, "out", 5, "splines"),
            (2, "out", 5, "ground"),
            (5, "out", 3, "splines"),
        ]);
    } else {
        edges.push((1, "out", 3, "splines"));
    }
    graph(nodes, &edges).to_ron()
}

fn hill_map() -> regia_terrain::TerrainMap {
    let n = 129u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32, (i / n) as f32);
            0.1 * (c - 64.0) + 0.3 * ((r - 64.0) * 0.4).sin()
        })
        .collect();
    regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0).expect("map")
}

fn terrain() -> regia_terrain::Terrain {
    regia_terrain::Terrain::new(
        &hill_map().to_heightfield(),
        256.0,
        100.0,
        -50.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain")
}

fn app(dir: Option<&std::path::Path>, terrain_edits: bool, flatten: bool) -> App {
    let files: BTreeMap<String, String> = [("roads.pgraph.ron".to_string(), road_graph(flatten))].into();
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        bake_dir: dir.map(std::path::Path::to_path_buf),
        terrain_edits,
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app
}

fn spawn_road(app: &mut App, from: Vec3, to: Vec3, guid: u128) {
    let mut road = Spline::through([from, to]);
    road.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    app.world_mut().spawn((
        road,
        RoadOf::default(),
        Tags::from_iter_str(["road"]),
        EntityGuid(Guid(guid)),
        Transform::default(),
        GlobalTransform::default(),
    ));
}

fn spawn_generator(app: &mut App, allow_terrain_edits: bool, tiles: Option<TileSettings>) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(100.0, 100.0, 100.0),
                },
                seed: Some(5),
                allow_terrain_edits,
                tiles,
                ..ProcGenerator::new("roads.pgraph.ron")
            },
            EntityGuid(Guid(0xabc)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

fn heights(app: &mut App) -> Vec<u32> {
    let w = app.world_mut();
    let t = w
        .query::<&regia_terrain::Terrain>()
        .iter(w)
        .next()
        .expect("terrain");
    t.map.heights().iter().map(|h| h.to_bits()).collect()
}

fn state(app: &App, e: Entity) -> (ProcGenRunState, bool) {
    let s = app.world().get::<ProcGenState>(e).expect("state");
    (s.state, s.stale)
}

/// Spec 64 § 9 test 13, world half: the bake changes heights only inside the rect, baking twice
/// equals baking once, its own edit does not mark it stale, an author's does, verify passes on the
/// flattened terrain, and clear restores every sample bit for bit.
#[test]
fn test_13_flatten_applies_restores_and_stays_current() {
    let dir = regia_test_utils::unique_temp_dir("procgen-flatten");
    let mut app = app(Some(&dir), true, true);
    app.world_mut().spawn(terrain());
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    let before = heights(&mut app);
    app.update(); // spawn change detection settles

    bake(app.world_mut(), e).expect("bake");
    let once = heights(&mut app);
    assert_ne!(once, before, "the terrain was flattened");
    let rec = app.world().get::<ProcOutput>(e).expect("output").0.terrain_edits[0].clone();
    let [c0, r0, c1, r1] = rec.rect;
    for (i, (a, b)) in once.iter().zip(&before).enumerate() {
        let (c, r) = (i as u32 % 129, i as u32 / 129);
        if a != b {
            assert!(
                (c0..c1).contains(&c) && (r0..r1).contains(&r),
                "sample {c},{r} changed outside the rect"
            );
        }
    }

    // The bake's own edit is noted for GPU upload and re-cook, and does not make it stale.
    {
        let w = app.world_mut();
        let edits = w
            .query::<&regia_terrain::TerrainEdits>()
            .iter(w)
            .next()
            .expect("the terrain records the edit");
        assert!(edits.generation() > 0);
    }
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        state(&app, e),
        (ProcGenRunState::Ready, false),
        "own edit: not stale"
    );

    // Baking again from the flattened terrain gives the same terrain: it never accumulates.
    let first_hash = app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    bake(app.world_mut(), e).expect("bake again");
    assert_eq!(heights(&mut app), once, "baking twice equals baking once");
    assert_eq!(
        app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash),
        first_hash
    );

    // Verify runs against the terrain without the generator's edit.
    let v = verify(app.world_mut(), e).expect("verify");
    assert!(v.ok, "{v:?}");
    assert_eq!(heights(&mut app), once, "verify leaves the terrain flattened");

    // An author's edit over the captured rect does make it stale.
    app.world_mut().write_message(ProcTerrainEdited {
        rect: [-5.0, -5.0, 5.0, 5.0],
        source: None,
    });
    app.update();
    assert!(state(&app, e).1, "an edit by someone else marks it stale");

    assert!(clear(app.world_mut(), e));
    assert_eq!(
        heights(&mut app),
        before,
        "clear restores every sample bit for bit"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gates fail the run and leave the terrain untouched.
#[test]
fn test_13_gates_leave_the_terrain_untouched() {
    for (project, generator) in [(false, true), (true, false)] {
        let mut app = app(None, project, true);
        app.world_mut().spawn(terrain());
        spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
        let e = spawn_generator(&mut app, generator, None);
        let before = heights(&mut app);
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Failed));
        let st = app.world().get::<ProcGenState>(e).expect("state");
        let (_, code, _) = st.last_error.clone().expect("error");
        assert_eq!(code, "terrain_edits_not_allowed");
        assert_eq!(heights(&mut app), before);
    }
}

/// A failed or cancelled re-run keeps the published ground: nothing was restored by the attempt.
#[test]
fn a_failed_rerun_keeps_the_applied_edit() {
    let mut app = app(None, true, true);
    app.world_mut().spawn(terrain());
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let flat = heights(&mut app);
    app.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .allow_terrain_edits = false;
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Failed));
    assert_eq!(heights(&mut app), flat, "the old output stays, with its ground");
    request_run(app.world_mut(), e);
    regia_procgen::world::cancel(app.world_mut(), e);
    assert_eq!(heights(&mut app), flat);
}

/// A bake's meshes and terrain edit come back from the file into a fresh world, a terrain that loads
/// after the generator gets the edit late, and clear still restores the originals.
#[test]
fn a_loaded_bake_applies_its_edit_and_publishes_its_meshes() {
    let dir = regia_test_utils::unique_temp_dir("procgen-load");
    let mut first = app(Some(&dir), true, true);
    first.world_mut().spawn(terrain());
    spawn_road(
        &mut first,
        Vec3::new(-60.0, 0.0, 0.0),
        Vec3::new(60.0, 0.0, 0.0),
        1,
    );
    let e = spawn_generator(&mut first, true, None);
    let before = heights(&mut first);
    bake(first.world_mut(), e).expect("bake");
    let flat = heights(&mut first);
    let baked = first.world().get::<ProcOutput>(e).expect("output").0.clone();
    assert!(baked.meshes.len() >= 2, "ribbon and markings");
    assert!(
        baked.meshes.iter().any(|m| !m.colors.is_empty()),
        "markings carry the profile's color"
    );

    // Fresh level: the generator first, the terrain a frame later (level streaming order).
    let mut second = app(Some(&dir), true, true);
    spawn_road(
        &mut second,
        Vec3::new(-60.0, 0.0, 0.0),
        Vec3::new(60.0, 0.0, 0.0),
        1,
    );
    let e2 = spawn_generator(&mut second, true, None);
    second.update();
    let out = second.world().get::<ProcOutput>(e2).expect("loaded").0.clone();
    assert_eq!(out.meshes, baked.meshes);
    assert_eq!(out.terrain_edits, baked.terrain_edits);
    second.world_mut().spawn(terrain());
    second.update();
    assert_eq!(heights(&mut second), flat, "the waiting edit applied");
    assert_eq!(
        second.world().get::<ProcGenState>(e2).map(|s| s.evaluations),
        Some(0)
    );
    // A terrain saved flattened is not written again, and clear still has the originals.
    load_bake(second.world_mut(), e2).expect("reload");
    assert!(clear(second.world_mut(), e2));
    assert_eq!(heights(&mut second), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Detach keeps the terrain edit (clearing afterwards has nothing to restore) and turns generated
/// meshes into mesh entities whose files the host's writer wrote.
#[test]
fn detach_keeps_the_terrain_edit_and_writes_mesh_entities() {
    use regia_procgen::world::ProcMeshWriter;
    use std::sync::Arc;
    let dir = regia_test_utils::unique_temp_dir("procgen-detach");
    let mut app = app(Some(&dir), true, true);
    app.world_mut().spawn(terrain());
    app.insert_resource(ProcMeshWriter(Arc::new(|mesh, name, path| {
        assert!(!mesh.indices.is_empty() && !name.is_empty());
        std::fs::create_dir_all(path.parent().expect("dir")).map_err(|e| e.to_string())?;
        std::fs::write(path, b"mesh").map_err(|e| e.to_string())
    })));
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    run_now(app.world_mut(), e);
    let flat = heights(&mut app);
    let meshes = app.world().get::<ProcOutput>(e).expect("output").0.meshes.len();
    let d = detach(app.world_mut(), e).expect("detach");
    assert_eq!(d.meshes.len(), meshes);
    assert_eq!(d.unsaved_meshes, 0);
    for m in &d.meshes {
        let source = app.world().get::<regia_render::MeshSource>(*m).expect("saved");
        assert!(std::path::Path::new(&source.mesh).is_file(), "{}", source.mesh);
        assert!(
            app.world().get::<regia_render::MeshRenderer>(*m).is_some(),
            "drawn at once"
        );
        assert!(
            app.world()
                .get::<regia_scene::components::Generated>(*m)
                .is_none()
        );
    }
    assert!(clear(app.world_mut(), e));
    assert_eq!(heights(&mut app), flat);
    let _ = std::fs::remove_dir_all(&dir);
}

fn triangles(m: &regia_procgen::GeneratedMesh) -> BTreeSet<[[u32; 3]; 3]> {
    m.indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let mut v = t.map(|i| m.positions[i as usize].map(f32::to_bits));
            v.sort_unstable();
            v
        })
        .collect()
}

/// Spec 66 § 2: a tiled road ribbon is cut between rows on the cell edge. Every cell's mesh sits in
/// that cell (by stretch midpoint), both halves share the edge row exactly, nothing is drawn twice,
/// and a tiled bake holds the meshes per cell.
#[test]
fn a_tiled_ribbon_splits_on_cell_edges_and_bakes_per_cell() {
    let dir = regia_test_utils::unique_temp_dir("procgen-tiled-road");
    let mut app = app(Some(&dir), true, false);
    app.insert_resource(regia_procgen::world::ProcGenTerrain(hill_map()));
    // Diagonal, so it crosses both X and Z edges of the 64 m grid.
    spawn_road(
        &mut app,
        Vec3::new(-90.0, 0.0, -71.0),
        Vec3::new(90.0, 0.0, 75.0),
        1,
    );
    let e = spawn_generator(
        &mut app,
        false,
        Some(TileSettings {
            cell_m: 64.0,
            ..TileSettings::default()
        }),
    );
    bake(app.world_mut(), e).expect("bake");
    let st = app.world().get::<ProcGenState>(e).expect("state");
    let run = st.tiled.clone().expect("tiled run");
    let mut all: BTreeSet<[[u32; 3]; 3]> = BTreeSet::new();
    let mut count = 0usize;
    let mut with_meshes = 0;
    for c in run.cells.values() {
        if !c.output.meshes.is_empty() {
            with_meshes += 1;
        }
        for m in &c.output.meshes {
            let t = triangles(m);
            count += t.len();
            all.extend(t);
        }
    }
    assert!(with_meshes >= 3, "the road crosses several cells: {with_meshes}");
    assert_eq!(all.len(), count, "no triangle in two cells");

    // Neighboring cells share vertices bit for bit on the seam.
    let ribbon = |k: &TileKey| {
        run.cells[k]
            .output
            .meshes
            .iter()
            .find(|m| m.material == "road.material.ron")
    };
    let keys: Vec<TileKey> = run
        .cells
        .keys()
        .copied()
        .filter(|k| ribbon(k).is_some())
        .collect();
    let mut shared = 0;
    for w in keys.windows(2) {
        let (a, b) = (ribbon(&w[0]).expect("a"), ribbon(&w[1]).expect("b"));
        let pa: BTreeSet<[u32; 3]> = a.positions.iter().map(|p| p.map(f32::to_bits)).collect();
        shared += b
            .positions
            .iter()
            .filter(|p| pa.contains(&p.map(f32::to_bits)))
            .count();
    }
    assert!(shared >= 5, "a row of five vertices on a seam: {shared}");
    // The run report adds the cells up: node lines exist, and the road is listed once.
    let report = app
        .world()
        .get::<ProcGenState>(e)
        .and_then(|s| s.report.clone())
        .expect("report");
    assert_eq!(report.roads.len(), 1);
    assert!(report.nodes.iter().any(|n| n.ty == "road.ribbon" && n.items > 0));

    // One cell re-baked: other cells keep their bytes (spec 66 § 8 test 11, with meshes).
    let path = bake_path(&dir, Guid(0xabc));
    let bytes = std::fs::read(&path).expect("file");
    let (_, cells) = regia_procgen::pgout::read_table(&bytes).expect("table");
    let target = cells[1];
    bake_cells(
        app.world_mut(),
        e,
        &[TileKey {
            tier: target.tier,
            cell: regia_procgen::tiles::CellCoord::new(i64::from(target.i), i64::from(target.j)),
        }],
    )
    .expect("re-bake");
    let after = std::fs::read(&path).expect("file");
    assert_eq!(after, bytes, "same inputs: same bytes");
    assert!(verify(app.world_mut(), e).expect("verify").ok);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 66 § 2: a baked tiled flatten records in each cell only the samples inside it, and together
/// they write what an untiled bake writes.
#[test]
fn a_tiled_flatten_records_the_part_inside_each_cell() {
    let run_one = |tiles: Option<TileSettings>| {
        let mut app = app(None, true, true);
        app.world_mut().spawn(terrain());
        spawn_road(&mut app, Vec3::new(-60.0, 0.0, 3.0), Vec3::new(60.0, 0.0, 3.0), 1);
        let e = spawn_generator(&mut app, true, tiles);
        let before = heights(&mut app);
        assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
        let after = heights(&mut app);
        let out = app.world().get::<ProcOutput>(e).expect("output").0.clone();
        (before, after, out.terrain_edits.len())
    };
    let (before, whole, whole_records) = run_one(None);
    let (_, tiled, tiled_records) = run_one(Some(TileSettings {
        cell_m: 64.0,
        ..TileSettings::default()
    }));
    assert_eq!(whole_records, 1);
    assert!(tiled_records > 1, "one record per touched cell: {tiled_records}");
    let differ = whole.iter().zip(&tiled).filter(|(a, b)| a != b).count();
    let changed = whole.iter().zip(&before).filter(|(a, b)| a != b).count();
    assert!(changed > 100);
    assert!(
        differ * 20 <= changed,
        "tiled rows differ only near cell edges: {differ} of {changed}"
    );
}

/// Spec 66 § 2: a terrain edit marks the cells around it, the next run evaluates only those, and the
/// result equals a full run of the edited terrain.
#[test]
fn a_terrain_edit_re_runs_only_the_cells_it_touches() {
    use regia_procgen::world::ProcGenTerrain;
    let files: BTreeMap<String, String> = [(
        "g.pgraph.ron".to_string(),
        graph(
            vec![
                (1, node("in.self", &[])),
                (
                    2,
                    node("sample.ground", &[("spacing_m", "6.0"), ("jitter", "0.5")]),
                ),
                (
                    3,
                    node("spawn.mesh", &[("entries", "[(mesh: \"rock\", weight: 1.0)]")]),
                ),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        )
        .to_ron(),
    )]
    .into();
    let setup = |edited: bool| {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let mut map = hill_map();
        if edited {
            bump(&mut map);
        }
        app.insert_resource(ProcGenTerrain(map));
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(96.0, 100.0, 96.0),
                    },
                    seed: Some(5),
                    tiles: Some(TileSettings {
                        cell_m: 32.0,
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(Guid(0xdef)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id();
        (app, e)
    };
    // The edit: a bump around (40, 40), inside the 32 m cell (1, 1).
    fn bump(map: &mut regia_terrain::TerrainMap) {
        let [col, row] = [84u32, 84]; // 2 m per sample, the map centered on the origin
        let rect = regia_terrain::DirtyRect {
            col_start: col - 1,
            col_end: col + 2,
            row_start: row - 1,
            row_end: row + 2,
        };
        regia_terrain::edit::write_height_samples(map, rect, &[9.0; 9]);
    }
    let (mut app, e) = setup(false);
    app.update();
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    app.update();
    let before = app
        .world()
        .get::<ProcGenState>(e)
        .and_then(|s| s.tiled.clone())
        .expect("run");
    assert_eq!(before.cells.len(), 49, "cells touching the closed bounds");
    bump(&mut app.world_mut().resource_mut::<ProcGenTerrain>().0);
    app.world_mut().write_message(ProcTerrainEdited {
        rect: [38.0, 38.0, 42.0, 42.0],
        source: None,
    });
    app.update();
    let st = app.world().get::<ProcGenState>(e).expect("state");
    assert!(st.stale && !st.stale_whole);
    let stale = st.stale_cells.clone();
    assert!((1..=4).contains(&stale.len()), "{stale:?}");
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let after = app
        .world()
        .get::<ProcGenState>(e)
        .and_then(|s| s.tiled.clone())
        .expect("run");
    let st = app.world().get::<ProcGenState>(e).expect("state");
    assert!(st.stale_cells.is_empty(), "the run cleared the marks");
    for (k, c) in &after.cells {
        if !stale.contains(k) {
            assert_eq!(
                c.output_hash, before.cells[k].output_hash,
                "cell {k:?} kept its output"
            );
        }
    }
    assert!(
        stale
            .iter()
            .any(|k| after.cells[k].output_hash != before.cells[k].output_hash),
        "the edited cell changed"
    );
    // The same as running every cell over the edited terrain.
    let (mut full, e2) = setup(true);
    assert_eq!(run_now(full.world_mut(), e2), Some(ProcGenRunState::Ready));
    let hash = |app: &App, e: Entity| app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    assert_eq!(hash(&app, e), hash(&full, e2));
}

/// The starter road network through a world: a crossing in a tiled generator makes one patch, in the
/// cell holding its centroid, and the tiled report counts it once.
#[test]
fn the_starter_network_makes_one_patch_in_a_tiled_run() {
    let profile = "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), samples_per_m: 0.5, material: \"road\", \
                   junction: (margin_m: 1.0, material: Some(\"patch\")))";
    let files: BTreeMap<String, String> = [
        (
            "roads.pgraph.ron".to_string(),
            regia_procgen::road::STARTER_ROAD_NETWORK_GRAPH.to_string(),
        ),
        ("two_lane.road.ron".to_string(), profile.to_string()),
    ]
    .into();
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(regia_procgen::world::ProcGenTerrain(hill_map()));
    for (k, (from, to)) in [
        (Vec3::new(-90.0, 0.0, 10.0), Vec3::new(90.0, 0.0, 20.0)),
        (Vec3::new(10.0, 0.0, -90.0), Vec3::new(30.0, 0.0, 90.0)),
    ]
    .into_iter()
    .enumerate()
    {
        let mut road = Spline::through([from, to]);
        road.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
        app.world_mut().spawn((
            road,
            RoadOf::new("two_lane.road.ron"),
            Tags::from_iter_str(["road"]),
            EntityGuid(Guid(k as u128 + 1)),
            Transform::default(),
            GlobalTransform::default(),
        ));
    }
    let e = spawn_generator(
        &mut app,
        false,
        Some(TileSettings {
            cell_m: 64.0,
            ..TileSettings::default()
        }),
    );
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let st = app.world().get::<ProcGenState>(e).expect("state");
    let run = st.tiled.clone().expect("tiled");
    let patches: Vec<&TileKey> = run
        .cells
        .iter()
        .filter(|(_, c)| c.output.meshes.iter().any(|m| m.material == "patch"))
        .map(|(k, _)| k)
        .collect();
    assert_eq!(patches.len(), 1, "{patches:?}");
    let report = st.report.clone().expect("report");
    let counted: u64 = report
        .nodes
        .iter()
        .filter_map(|n| n.counters.get("junctions"))
        .sum();
    assert_eq!(counted, 1, "one junction over all cells");
    assert!(report.roads.values().all(|r| r.junctions == 1));
    assert!(
        app.world()
            .get::<ProcOutput>(e)
            .expect("output")
            .0
            .meshes
            .iter()
            .any(|m| m.material == "patch"),
        "the merged output keeps the patch"
    );
}

/// Spec 64 § 7.3: a run and a clear each log one change, an undo through `set_records` restores
/// the ground bit for bit and logs nothing, a redo brings the edit back, and an undone output goes
/// stale. A re-run staleness started logs nothing, and a `live` generator stays undone.
#[test]
fn terrain_changes_log_once_and_replay_bit_for_bit() {
    use regia_procgen::terrain_edits::{ProcTerrainEditLog, set_records};
    let mut app = app(None, true, true);
    app.world_mut().spawn(terrain());
    app.world_mut().init_resource::<ProcTerrainEditLog>();
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    let ground = heights(&mut app);
    app.update();
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let flat = heights(&mut app);
    assert_ne!(flat, ground, "the road flattened the ground");
    let change = {
        let mut log = app.world_mut().resource_mut::<ProcTerrainEditLog>();
        assert_eq!(log.0.len(), 1, "one publish, one change");
        log.0.remove(0)
    };
    assert!(change.before.is_empty() && !change.after.is_empty());
    assert_eq!(change.generator, Guid(0xabc));

    set_records(app.world_mut(), e, &change.before).expect("undo");
    assert_eq!(heights(&mut app), ground, "undo restores every sample");
    assert!(
        !state(&app, e).1,
        "a stale live generator would re-flatten at once"
    );
    set_records(app.world_mut(), e, &change.after).expect("redo");
    assert_eq!(heights(&mut app), flat, "redo brings the edit back");
    app.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .live = false;
    set_records(app.world_mut(), e, &change.before).expect("undo");
    assert!(state(&app, e).1, "the output no longer matches the ground");
    set_records(app.world_mut(), e, &change.after).expect("redo");
    assert_eq!(heights(&mut app), flat, "redo brings the edit back");
    assert!(
        app.world().resource::<ProcTerrainEditLog>().0.is_empty(),
        "an undo is not an edit"
    );

    // Re-running to the same records changes nothing, so it is no entry either.
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    assert!(app.world().resource::<ProcTerrainEditLog>().0.is_empty());

    assert!(regia_procgen::world::clear(app.world_mut(), e));
    let cleared = app.world_mut().resource_mut::<ProcTerrainEditLog>().0.remove(0);
    assert_eq!(cleared.before, change.after);
    assert!(cleared.after.is_empty());
    assert_eq!(heights(&mut app), ground);

    // Moving the road makes the generator stale, and the re-run it starts by itself is no entry.
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    app.world_mut().resource_mut::<ProcTerrainEditLog>().0.clear();
    app.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .refresh_on_change = true;
    let w = app.world_mut();
    let mut road = w.query::<&mut Spline>().single_mut(w).expect("road");
    road.points[0].position.z = 8.0;
    for _ in 0..4 {
        app.update();
    }
    assert_ne!(heights(&mut app), flat, "the re-run moved the flatten");
    assert!(app.world().resource::<ProcTerrainEditLog>().0.is_empty());
}

/// Sample `(col, row)` of the test terrain as an index into [`heights`].
fn at(col: u32, row: u32) -> usize {
    (row * 129 + col) as usize
}

/// Sculpts one sample the way the editor's terrain tools do, through `TerrainEdits`.
fn sculpt(app: &mut App, col: u32, row: u32, height: f32) {
    let w = app.world_mut();
    let e = w
        .query_filtered::<Entity, With<regia_terrain::Terrain>>()
        .iter(w)
        .next()
        .expect("terrain");
    if w.get::<regia_terrain::TerrainEdits>(e).is_none() {
        w.entity_mut(e).insert(regia_terrain::TerrainEdits::default());
    }
    let mut entity = w.entity_mut(e);
    let mut edits = entity.take::<regia_terrain::TerrainEdits>().expect("edits");
    let rect = regia_terrain::DirtyRect {
        col_start: col,
        col_end: col + 1,
        row_start: row,
        row_end: row + 1,
    };
    let mut t = entity.get_mut::<regia_terrain::Terrain>().expect("terrain");
    regia_terrain::edit::apply_height_patch(&mut t, &mut edits, rect, &[height]).expect("patch");
    entity.insert(edits);
}

/// The record's rectangle is a bounding box: a diagonal road's holds ground the road never touched.
/// A sculpt there stays through re-runs and a clear, reaches the capture and marks the generator
/// stale; a sculpt on the road goes under the next flatten and comes back with the clear.
#[test]
fn an_author_sculpt_inside_the_edit_rectangle_survives() {
    let mut app = app(None, true, true);
    app.world_mut().spawn(terrain());
    spawn_road(
        &mut app,
        Vec3::new(-60.0, 0.0, -60.0),
        Vec3::new(60.0, 0.0, 60.0),
        1,
    );
    let e = spawn_generator(&mut app, true, None);
    let ground = heights(&mut app);
    app.update();
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    assert!(!state(&app, e).1);
    let flat = heights(&mut app);
    let out = app.world().get::<ProcOutput>(e).expect("output").0.clone();
    let [c0, r0, c1, r1] = out.terrain_edits[0].rect;
    // The far corner of the box, away from the road, and a sample on the road.
    let (off_col, off_row) = (c1 - 1, r0);
    assert_eq!(
        flat[at(off_col, off_row)],
        ground[at(off_col, off_row)],
        "untouched"
    );
    let (on_col, on_row) = ((c0 + c1) / 2, (r0 + r1) / 2);
    assert_ne!(
        flat[at(on_col, on_row)],
        ground[at(on_col, on_row)],
        "under the road"
    );

    sculpt(&mut app, off_col, off_row, 40.0);
    app.update();
    assert!(
        state(&app, e).1,
        "an author's sculpt under the bounds marks it stale"
    );
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    assert_eq!(heights(&mut app)[at(off_col, off_row)], 40f32.to_bits());

    // A dent: a road rides over a spike, since its surface never dips under the ground.
    let dent = f32::from_bits(flat[at(on_col, on_row)]) - 1.0;
    sculpt(&mut app, on_col, on_row, dent);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let reflat = heights(&mut app);
    assert_ne!(reflat[at(on_col, on_row)], dent.to_bits(), "the road goes on top");
    let terrain_edits = app
        .world()
        .get::<ProcOutput>(e)
        .expect("output")
        .0
        .terrain_edits
        .clone();
    let original = terrain_edits[0].original[((on_row - r0) * (c1 - c0) + (on_col - c0)) as usize];
    assert_eq!(original, dent, "the capture saw the sculpt, not the road");

    assert!(clear(app.world_mut(), e));
    let mut expected = ground;
    expected[at(off_col, off_row)] = 40f32.to_bits();
    expected[at(on_col, on_row)] = dent.to_bits();
    assert_eq!(heights(&mut app), expected);
}

/// The same road network with a higher surface: paired with `road_graph(true)` it flattens the
/// same samples to a different height, so two generators' edits overlap with different values.
fn road_graph_high() -> String {
    let profile = PROFILE.replace("surface_lift_m: 0.05", "surface_lift_m: 0.4");
    assert_ne!(profile, PROFILE);
    let nodes = vec![
        (
            1,
            node(
                "in.entities",
                &[
                    ("tags", "[\"road\"]"),
                    ("as", "\"splines\""),
                    ("within", "\"world\""),
                ],
            ),
        ),
        (2, node("in.ground", &[])),
        (3, node("terrain.flatten_road", &[("profile", &profile)])),
    ];
    let edges = vec![(2, "out", 3, "ground"), (1, "out", 3, "splines")];
    graph(nodes, &edges).to_ron()
}

/// Determinism audit finding 1: two concurrent runs whose terrain edits own the same sample
/// resolve by `EntityGuid`, not by the ECS query order or by which threaded task finishes first.
/// The lower guid's edit stays applied and the higher guid's run fails `terrain_edit_overlap` —
/// in both spawn/archetype orders (`Unrelated` moves an entity to another archetype table, which
/// used to flip the winner) and with threaded evaluation.
#[test]
fn overlapping_terrain_edits_keep_the_lower_guid() {
    #[derive(Component)]
    struct Unrelated;

    let build = |threaded: bool| {
        let files: BTreeMap<String, String> = [
            ("roads.pgraph.ron".to_string(), road_graph(true)),
            ("roads_high.pgraph.ron".to_string(), road_graph_high()),
        ]
        .into();
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: !threaded,
            source: ProcGraphSource::Memory(files),
            terrain_edits: true,
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app
    };
    let spawn = |app: &mut App, graph: &str, guid: u128| {
        app.world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(100.0, 100.0, 100.0),
                    },
                    allow_terrain_edits: true,
                    ..ProcGenerator::new(graph)
                },
                EntityGuid(Guid(guid)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id()
    };
    // Runs the scenario; with one of `with_low`/`with_high` false it is the control measuring
    // that generator's flatten alone.
    let scenario = |threaded: bool, moved: &str, with_low: bool, with_high: bool| {
        let mut app = build(threaded);
        app.world_mut().spawn(terrain());
        spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
        // The higher guid spawns first so the raw archetype query yields it before the lower one.
        let high = with_high.then(|| spawn(&mut app, "roads_high.pgraph.ron", 0xdef));
        let low = with_low.then(|| spawn(&mut app, "roads.pgraph.ron", 0xabc));
        match moved {
            "high" => {
                if let Some(high) = high {
                    app.world_mut().entity_mut(high).insert(Unrelated);
                }
            }
            "low" => {
                if let Some(low) = low {
                    app.world_mut().entity_mut(low).insert(Unrelated);
                }
            }
            _ => {}
        }
        if let Some(low) = low {
            request_run(app.world_mut(), low);
        }
        if let Some(high) = high {
            request_run(app.world_mut(), high);
        }
        let mut states = (None, None);
        for _ in 0..120 {
            app.update();
            let settled = |e: Entity| {
                matches!(
                    app.world().get::<ProcGenState>(e).map(|s| s.state),
                    Some(ProcGenRunState::Ready) | Some(ProcGenRunState::Failed)
                )
            };
            let low_settled = low.is_none_or(settled);
            let high_settled = high.is_none_or(settled);
            if low_settled && high_settled {
                break;
            }
        }
        if let Some(low) = low {
            let s = app.world().get::<ProcGenState>(low).expect("state");
            states.0 = Some((s.state, s.last_error.as_ref().map(|x| x.1.clone())));
        }
        if let Some(high) = high {
            let s = app.world().get::<ProcGenState>(high).expect("state");
            states.1 = Some((s.state, s.last_error.as_ref().map(|x| x.1.clone())));
        }
        (states, heights(&mut app))
    };

    for threaded in [false, true] {
        for moved in ["none", "high", "low"] {
            let (states, heights) = scenario(threaded, moved, true, true);
            let (low_state, low_err) = match states.0 {
                Some((s, e)) => (Some(s), e),
                None => (None, None),
            };
            let (high_state, high_err) = match states.1 {
                Some((s, e)) => (Some(s), e),
                None => (None, None),
            };
            assert_eq!(
                low_state,
                Some(ProcGenRunState::Ready),
                "threaded {threaded} moved {moved}: the lower guid applies"
            );
            assert_eq!(
                high_state,
                Some(ProcGenRunState::Failed),
                "threaded {threaded} moved {moved}"
            );
            assert_eq!(
                high_err.as_deref(),
                Some("terrain_edit_overlap"),
                "threaded {threaded} moved {moved}"
            );
            let _ = low_err;
            let (_, low_alone) = scenario(threaded, moved, true, false);
            let (_, high_alone) = scenario(threaded, moved, false, true);
            assert_ne!(
                low_alone, high_alone,
                "threaded {threaded} moved {moved}: the two flattens differ, so the test can tell who won"
            );
            assert_eq!(
                heights, low_alone,
                "threaded {threaded} moved {moved}: the lower guid's flatten stays"
            );
        }
    }
}

/// Fixrev-17 (Muse) review of the procgen lane, finding 1: the deferral in `poll` only sees
/// runs that already hold a job, and the queue used to start runs in request order. With one
/// concurrent slot (the Android default) a higher-guid run requested before a lower-guid one
/// took the slot, applied, and owned the samples, while a four-slot desktop started both and
/// the lower guid won — the same level and requests produced different heightfields per
/// platform. The queue now starts in guid order; this pins the one-slot, reversed-request
/// case: the lower guid must still apply and the higher guid must fail
/// `terrain_edit_overlap`.
#[test]
fn queued_runs_start_in_guid_order_with_one_slot() {
    let files: BTreeMap<String, String> = [
        ("roads.pgraph.ron".to_string(), road_graph(true)),
        ("roads_high.pgraph.ron".to_string(), road_graph_high()),
    ]
    .into();
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: false,
        source: ProcGraphSource::Memory(files),
        terrain_edits: true,
        max_concurrent_runs: 1,
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut().spawn(terrain());
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    // The higher guid spawns first, and — unlike
    // `overlapping_terrain_edits_keep_the_lower_guid` — is also *requested* first: request
    // order is the order that used to decide the single slot.
    let high = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(100.0, 100.0, 100.0),
                },
                allow_terrain_edits: true,
                ..ProcGenerator::new("roads_high.pgraph.ron")
            },
            EntityGuid(Guid(0xdef)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let low = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(100.0, 100.0, 100.0),
                },
                allow_terrain_edits: true,
                ..ProcGenerator::new("roads.pgraph.ron")
            },
            EntityGuid(Guid(0xabc)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let settled = |app: &mut App, e: Entity| {
        matches!(
            app.world().get::<ProcGenState>(e).map(|s| s.state),
            Some(ProcGenRunState::Ready) | Some(ProcGenRunState::Failed)
        )
    };
    request_run(app.world_mut(), high);
    request_run(app.world_mut(), low);
    for _ in 0..240 {
        app.update();
        if settled(&mut app, high) && settled(&mut app, low) {
            break;
        }
    }
    let low_state = app.world().get::<ProcGenState>(low).expect("low state");
    assert_eq!(
        low_state.state,
        ProcGenRunState::Ready,
        "the lower guid owns the samples even with one slot and reversed request order"
    );
    let high_state = app.world().get::<ProcGenState>(high).expect("high state");
    assert_eq!(
        high_state.state,
        ProcGenRunState::Failed,
        "the higher guid's overlapping edit fails"
    );
    assert_eq!(
        high_state.last_error.as_ref().map(|x| x.1.clone()),
        Some("terrain_edit_overlap".to_string())
    );
}

/// Clearing a generator whose originals hold another's edit would leave that edit behind for good.
#[test]
fn a_second_generator_may_not_edit_the_same_samples() {
    let mut app = app(None, true, true);
    app.world_mut().spawn(terrain());
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let first = spawn_generator(&mut app, true, None);
    let second = spawn_generator(&mut app, true, None);
    app.world_mut().entity_mut(second).insert(EntityGuid(Guid(0xdef)));
    let ground = heights(&mut app);
    assert_eq!(run_now(app.world_mut(), first), Some(ProcGenRunState::Ready));
    let flat = heights(&mut app);
    assert_eq!(run_now(app.world_mut(), second), Some(ProcGenRunState::Failed));
    let st = app.world().get::<ProcGenState>(second).expect("state");
    let (_, code, _) = st.last_error.clone().expect("error");
    assert_eq!(code, "terrain_edit_overlap");
    assert_eq!(heights(&mut app), flat, "nothing was written");
    assert!(clear(app.world_mut(), first));
    assert_eq!(heights(&mut app), ground);
    assert_eq!(run_now(app.world_mut(), second), Some(ProcGenRunState::Ready));
    assert!(clear(app.world_mut(), second));
    assert_eq!(heights(&mut app), ground);
}

/// A terrain replaced under an applied edit keeps its own heights: neither a capture nor a clear may
/// stamp the old records on it, even when the size still fits.
#[test]
fn a_replaced_terrain_of_the_same_size_is_left_alone() {
    let mut app = app(None, true, true);
    let t = app.world_mut().spawn(terrain()).id();
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let raised: Vec<f32> = hill_map().heights().iter().map(|h| h + 5.0).collect();
    let replaced =
        regia_terrain::TerrainMap::from_heights(129, 129, raised, 256.0, 100.0, -50.0).expect("map");
    app.world_mut()
        .get_mut::<regia_terrain::Terrain>(t)
        .expect("terrain")
        .map = replaced;
    let new_ground = heights(&mut app);
    assert!(clear(app.world_mut(), e));
    assert_eq!(heights(&mut app), new_ground);
}

/// Records from a file are checked before a sample moves: a NaN original would reach the map.
#[test]
fn a_record_with_a_bad_original_is_refused() {
    let mut app = app(None, true, true);
    app.world_mut().spawn(terrain());
    let e = spawn_generator(&mut app, true, None);
    let ground = heights(&mut app);
    let record = regia_procgen::exec::TerrainEditRecord {
        rect: [0, 0, 1, 1],
        samples: vec![1.0],
        original: vec![f32::NAN],
    };
    let (code, _) =
        regia_procgen::terrain_edits::publish(app.world_mut(), e, &[record]).expect_err("refused");
    assert_eq!(code, "terrain_edit_invalid");
    assert_eq!(heights(&mut app), ground);
}

/// Spec 66 § 2 keeps a run partial only when the halo covers what a cell reads. A flattened sample
/// takes its target from the road surface, which can be farther than the halo: a bump under the road
/// in cell column 1 changes the flatten in column 0, so road graphs re-run every cell.
#[test]
fn a_terrain_edit_under_a_road_matches_a_full_run() {
    let tiles = || {
        Some(TileSettings {
            cell_m: 64.0,
            ..TileSettings::default()
        })
    };
    let setup = |bumped: bool| {
        let mut a = app(None, true, true);
        let mut t = terrain();
        if bumped {
            let rect = regia_terrain::DirtyRect {
                col_start: 99,
                col_end: 100,
                row_start: 79,
                row_end: 80,
            };
            regia_terrain::edit::write_height_samples(&mut t.map, rect, &[2.0]);
        }
        a.world_mut().spawn(t);
        spawn_road(&mut a, Vec3::new(66.0, 0.0, -60.0), Vec3::new(66.0, 0.0, 60.0), 1);
        let e = spawn_generator(&mut a, true, tiles());
        (a, e)
    };
    let (mut edited, e) = setup(false);
    edited.update();
    assert_eq!(run_now(edited.world_mut(), e), Some(ProcGenRunState::Ready));
    sculpt(&mut edited, 99, 79, 2.0);
    edited.update();
    assert!(state(&edited, e).1);
    assert_eq!(run_now(edited.world_mut(), e), Some(ProcGenRunState::Ready));
    let (mut full, e2) = setup(true);
    assert_eq!(run_now(full.world_mut(), e2), Some(ProcGenRunState::Ready));
    let hash = |a: &App, e: Entity| a.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    assert_eq!(hash(&edited, e), hash(&full, e2));
}

/// `procgen.bake { cells }` must not add a cell the plan does not own: a merged load would then
/// publish its output on top of the real cells.
#[test]
fn rebaking_cells_checks_the_tier_and_the_cell() {
    use regia_procgen::tiles::CellCoord;
    let dir = regia_test_utils::unique_temp_dir("procgen-rebake-keys");
    let mut app = app(Some(&dir), false, false);
    app.world_mut().spawn(terrain());
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 3.0), Vec3::new(60.0, 0.0, 3.0), 1);
    let e = spawn_generator(
        &mut app,
        false,
        Some(TileSettings {
            cell_m: 64.0,
            ..TileSettings::default()
        }),
    );
    let path = bake(app.world_mut(), e).expect("bake");
    let before = std::fs::read(&path).expect("file");
    let key = |tier: u8, x: i64, z: i64| TileKey {
        tier,
        cell: CellCoord::new(x, z),
    };
    let (code, _) = bake_cells(app.world_mut(), e, &[key(9, 0, 0)]).expect_err("tier");
    assert_eq!(code, "procgen_tier_unknown");
    let (code, _) = bake_cells(app.world_mut(), e, &[key(0, 500, 0)]).expect_err("cell");
    assert_eq!(code, "procgen_unknown_cell");
    assert_eq!(std::fs::read(&path).expect("file"), before, "nothing was written");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mesh that could not be saved lives only in the bake: the detach reports it and keeps the file.
#[test]
fn a_failed_mesh_write_keeps_the_bake() {
    use regia_procgen::world::ProcMeshWriter;
    use std::sync::Arc;
    let dir = regia_test_utils::unique_temp_dir("procgen-detach-unsaved");
    let mut app = app(Some(&dir), true, true);
    app.world_mut().spawn(terrain());
    app.insert_resource(ProcMeshWriter(Arc::new(|_, _, _| Err("disk full".to_string()))));
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, true, None);
    let path = bake(app.world_mut(), e).expect("bake");
    let d = detach(app.world_mut(), e).expect("detach");
    assert!(d.unsaved_meshes > 0);
    assert!(path.is_file(), "the only copy of the unsaved meshes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A detached mesh is level content: its path must work on another machine, so it is relative to
/// the project with forward slashes (spec 62 § 7.3).
#[test]
fn detached_meshes_name_project_relative_paths() {
    use regia_procgen::world::{ProcMeshWriter, level_bake_dir};
    use std::sync::Arc;
    let dir = regia_test_utils::unique_temp_dir("procgen-detach-relative");
    std::fs::write(dir.join("roads.pgraph.ron"), road_graph(false)).expect("graph");
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Dir(dir.clone()),
        bake_dir: Some(level_bake_dir(&dir.join("assets/maps/main.level.ron"))),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut().spawn(terrain());
    app.insert_resource(ProcMeshWriter(Arc::new(|_, _, path| {
        std::fs::create_dir_all(path.parent().expect("dir")).map_err(|e| e.to_string())?;
        std::fs::write(path, b"mesh").map_err(|e| e.to_string())
    })));
    spawn_road(&mut app, Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0), 1);
    let e = spawn_generator(&mut app, false, None);
    assert_eq!(run_now(app.world_mut(), e), Some(ProcGenRunState::Ready));
    let d = detach(app.world_mut(), e).expect("detach");
    assert!(!d.meshes.is_empty());
    for m in &d.meshes {
        let source = app.world().get::<regia_render::MeshSource>(*m).expect("saved");
        assert!(
            source.mesh.starts_with("assets/maps/main.procgen/detached/"),
            "{}",
            source.mesh
        );
        assert!(dir.join(&source.mesh).is_file(), "{}", source.mesh);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
