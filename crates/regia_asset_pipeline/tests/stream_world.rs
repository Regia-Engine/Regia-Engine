// SPDX-License-Identifier: MPL-2.0
//! A streamed level through the real path: `cook_project` writes the cells, the always-loaded set and
//! the index into the cooked cache; `open_streamed_level` reads them through the asset server's
//! payload reader on the IO pool. Spec 78 § 18 test 11's path, with lockstep timing for fixed steps.

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer, AssetServerSettings, cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::ScenePlugin;
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Name};
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{CellMember, SourceKind, StreamingSource};
use regia_world::format::{WorldIndex, world_index_id};
use regia_world::{
    CellState, StreamTiming, WorldOrigin, WorldStreaming, WorldStreamingPlugin, open_streamed_level,
};

const STREAMING: &str = "regia_scene::streaming::LevelStreaming";
const TRANSFORM: &str = "regia_scene::transform::Transform";

/// A project with one streamed level: a row of eight 100 m cells far from the origin, and one entity
/// that stays with the level.
fn project(name: &str) -> (std::path::PathBuf, LevelId) {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets/maps")).expect("dirs");
    let mut entities = String::new();
    for x in 0..8u32 {
        entities.push_str(&format!(
            "(guid: \"{}\", name: \"prop{x}\", components: {{ \"{TRANSFORM}\": (translation: ({}, 1.0, 50.0)) }}),",
            Guid::from_u128(u128::from(100 + x)),
            f64::from(x) * 100.0 + 50.0 + 6000.0
        ));
    }
    entities.push_str(&format!(
        "(guid: \"{}\", name: \"rules\", components: {{ }}),",
        Guid::from_u128(9)
    ));
    let text = format!(
        "(format_version: 1, entities: [{entities}], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: \
         100.0, load_radius_m: 40.0, unload_radius_m: 60.0, hlod: None) }})"
    );
    std::fs::write(dir.join("assets/maps/open.level.ron"), text).expect("write");
    let report = cook_project(CookOptions::for_host(dir.clone())).expect("cook");
    let fatal: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.contains("unsupported source extension"))
        .collect();
    assert!(fatal.is_empty(), "{fatal:?}");
    let meta = std::fs::read(dir.join("assets/maps/open.level.ron.meta")).expect("meta");
    let meta = regia_asset::MetaSidecar::from_ron(&meta).expect("parse");
    (dir, LevelId(meta.id.0))
}

fn app(dir: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
    regia_app::enable_fixed_step(&mut app, 60.0);
    let server = AssetServer::open(AssetServerSettings {
        project_root: dir.to_path_buf(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    app.insert_resource(server);
    app
}

#[test]
fn a_cooked_level_streams_through_the_asset_server() {
    let (dir, level) = project("stream_world_e2e");
    let platform = regia_asset_pipeline::host_platform();
    let cache = cooked_cache_dir(&dir, platform);
    let index_bytes = std::fs::read(cache.join(cooked_file_name(world_index_id(level)))).expect("index");
    let index = WorldIndex::from_bytes(&index_bytes, level).expect("decode");
    assert_eq!(index.cells.len(), 8);

    let mut app = app(&dir);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    // A level nothing cooked is left to the ordinary open.
    assert_eq!(
        regia_world::open_if_cooked(
            app.world_mut(),
            LevelId(regia_core::id::Guid::from_u128(1)),
            LoadMode::Replace
        ),
        regia_scene::streaming::StreamedOpen::NotCooked
    );
    let always = open_streamed_level(app.world_mut(), level, LoadMode::Replace).expect("open");
    assert_eq!(always, 1, "the entity with no transform stays with the level");

    let source = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(6250.0, 0.0, 50.0)),
            StreamingSource {
                kind: SourceKind::Player,
                ..StreamingSource::default()
            },
        ))
        .id();
    for _ in 0..8 {
        app.update();
    }
    let cell = CellCoord::new(62, 0);
    let ws = app.world().resource::<WorldStreaming>();
    assert_eq!(ws.state(level, cell), Some(CellState::Visible));
    assert_eq!(ws.stats().failures, 0);

    // The source started past the 5 km threshold, so the origin moved to its cell.
    let origin = *app.world().resource::<WorldOrigin>();
    assert_eq!(origin.0.cell(), [6, 0, 0]);
    assert_eq!(app.world().resource::<WorldStreaming>().stats().rebases, 1);

    // The prop keeps its world position through the anchor and the rebase.
    let wanted = Guid::from_u128(102);
    let world = app.world_mut();
    let mut query = world.query::<(&EntityGuid, &Transform, &CellMember)>();
    let (_, transform, member) = query
        .iter(world)
        .find(|(guid, _, _)| guid.0 == wanted)
        .expect("the prop spawned");
    assert_eq!(member.0, cell);
    assert_eq!(transform.translation, Vec3::new(106.0, 1.0, 50.0));
    assert_eq!(
        origin.to_world(transform.translation.to_array()),
        [6250.0, 1.0, 50.0]
    );

    // At the far end the first cell is gone and the last one is there.
    let local = origin.from_world([6750.0, 0.0, 50.0]).expect("local");
    app.world_mut()
        .get_mut::<Transform>(source)
        .expect("source")
        .translation = Vec3::from_array(local);
    for _ in 0..8 {
        app.update();
    }
    let ws = app.world().resource::<WorldStreaming>();
    assert_eq!(ws.state(level, cell), Some(CellState::Unloaded));
    assert_eq!(ws.state(level, CellCoord::new(67, 0)), Some(CellState::Visible));
}

/// Spec 78 § 18 test 11: async timing over a pak, with the cooked cache gone, so every byte comes from
/// the mapped pak. Bounded by a step count; nothing here reads a clock.
#[test]
fn a_packed_level_streams_in_async_timing_with_no_main_thread_wait() {
    let (dir, level) = project("stream_world_pak");
    let cache = cooked_cache_dir(&dir, regia_asset_pipeline::host_platform());
    let content = dir.join("content");
    std::fs::create_dir_all(&content).expect("content dir");
    regia_asset_pipeline::pak_writer::pack_cooked_directory(&cache, &content, &Default::default(), true)
        .expect("pak");
    std::fs::remove_dir_all(&cache).expect("drop the cache");
    std::fs::create_dir_all(&cache).expect("empty cache");

    let mut app = App::new();
    app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
    regia_app::enable_fixed_step(&mut app, 60.0);
    let server = AssetServer::open(AssetServerSettings {
        project_root: dir.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: Some(content),
    })
    .expect("server");
    app.insert_resource(server);
    open_streamed_level(app.world_mut(), level, LoadMode::Replace).expect("open");
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(6250.0, 0.0, 50.0)),
        StreamingSource::default(),
    ));
    let cell = CellCoord::new(62, 0);
    for _ in 0..200_000 {
        app.update();
        let state = app.world().resource::<WorldStreaming>().state(level, cell);
        if state == Some(CellState::Visible) {
            break;
        }
        assert!(!matches!(state, Some(CellState::Failed(_))), "{state:?}");
        std::thread::yield_now();
    }
    let ws = app.world().resource::<WorldStreaming>();
    assert_eq!(ws.state(level, cell), Some(CellState::Visible));
    assert_eq!(ws.stats().main_thread_io_waits, 0);
}

#[test]
fn a_corrupt_cell_in_the_cache_fails_without_stopping_the_others() {
    let (dir, level) = project("stream_world_corrupt");
    let cache = cooked_cache_dir(&dir, regia_asset_pipeline::host_platform());
    let bad = regia_world::format::cell_blob_id(level, CellCoord::new(62, 0));
    let path = cache.join(cooked_file_name(bad));
    let mut bytes = std::fs::read(&path).expect("blob");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).expect("corrupt");

    let mut app = app(&dir);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open_streamed_level(app.world_mut(), level, LoadMode::Replace).expect("open");
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(6295.0, 0.0, 50.0)),
        StreamingSource::default(),
    ));
    for _ in 0..8 {
        app.update();
    }
    let ws = app.world().resource::<WorldStreaming>();
    assert_eq!(
        ws.state(level, CellCoord::new(62, 0)),
        Some(CellState::Failed("cell_corrupt".into()))
    );
    assert_eq!(ws.state(level, CellCoord::new(63, 0)), Some(CellState::Visible));
}

#[test]
fn a_newer_world_index_refuses_to_open_with_its_code() {
    let (dir, level) = project("stream_world_newer");
    let cache = cooked_cache_dir(&dir, regia_asset_pipeline::host_platform());
    let id: AssetId = world_index_id(level);
    let path = cache.join(cooked_file_name(id));
    let mut index = WorldIndex::from_bytes(&std::fs::read(&path).expect("read"), level).expect("decode");
    index.format_version += 1;
    std::fs::write(&path, index.to_bytes().expect("encode")).expect("write");
    let mut first = app(&dir);
    let error = open_streamed_level(first.world_mut(), level, LoadMode::Replace).expect_err("newer");
    assert_eq!(error.reason(), "stream_format_newer");
    // Spec 78 § 6.5: the level is open and the always-loaded set spawned. Cell streaming did not start.
    let names: Vec<String> = first
        .world_mut()
        .query::<&Name>()
        .iter(first.world())
        .map(|name| name.as_str().to_owned())
        .collect();
    assert!(names.iter().any(|name| name == "rules"), "{names:?}");
    assert!(
        !first.world().resource::<WorldStreaming>().is_active(),
        "a newer index does not open the cell runtime"
    );
    // A second app: the first open already holds the level, so `open_if_cooked` is measured fresh.
    let mut again = app(&dir);
    assert_eq!(
        regia_world::open_if_cooked(again.world_mut(), level, LoadMode::Replace),
        regia_scene::streaming::StreamedOpen::AlwaysLoaded
    );
}

/// Spec 78 § 13.2: with `nav` set the cook writes each nav tile into the cell its center falls in, a
/// cell with no entity joins the world for its tiles, and at run time the tiles come and go with their
/// cells, through a streamed cache built from the index.
#[test]
fn nav_tiles_cook_into_cells_and_stream_with_them() {
    let dir = regia_test_utils::unique_temp_dir("stream_world_nav");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("dirs");
    // Eight 100 m cells of ground, one prop in only the first cell.
    let text = format!(
        "(format_version: 1, entities: [\
         (guid: \"{}\", name: \"ground\", components: {{ \"{TRANSFORM}\": (translation: (400.0, -0.5, 50.0)), \
          \"regia_physics::components::Collider\": (shape: Box(half_extents: (400.0, 0.5, 50.0))), \
          \"regia_physics::components::RigidBody\": (kind: Static) }}),\
         (guid: \"{}\", name: \"prop\", components: {{ \"{TRANSFORM}\": (translation: (50.0, 1.0, 50.0)) }}),\
         ], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: 100.0, load_radius_m: 40.0, \
         unload_radius_m: 60.0, hlod: None, nav: Some((cell_size: 1.0, tile_size: 16))) }})",
        Guid::from_u128(1),
        Guid::from_u128(2),
    );
    std::fs::write(dir.join("assets/maps/open.level.ron"), text).expect("write");
    let report = cook_project(CookOptions::for_host(dir.clone())).expect("cook");
    let fatal: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.contains("unsupported source extension"))
        .collect();
    assert!(fatal.is_empty(), "{fatal:?}");
    let meta = std::fs::read(dir.join("assets/maps/open.level.ron.meta")).expect("meta");
    let level = LevelId(regia_asset::MetaSidecar::from_ron(&meta).expect("parse").id.0);

    let platform = regia_asset_pipeline::host_platform();
    let cache_dir = cooked_cache_dir(&dir, platform);
    let index_path = cache_dir.join(cooked_file_name(world_index_id(level)));
    let index_bytes = std::fs::read(&index_path).expect("index");
    let index = WorldIndex::from_bytes(&index_bytes, level).expect("decode");
    let layout = index.nav.expect("a layout");
    assert_eq!(layout.cook.tile_size, 16);
    assert!(
        index.cells.len() >= 8,
        "cells with only tiles join: {}",
        index.cells.len()
    );
    assert!(
        index.cells.iter().all(|c| c.nav_tiles > 0),
        "{:?}",
        index.cells.len()
    );

    // Cooking again gives the same bytes.
    cook_project(CookOptions::for_host(dir.clone())).expect("cook again");
    assert_eq!(std::fs::read(&index_path).expect("index"), index_bytes);

    let mut app = app(&dir);
    app.add_plugins(regia_nav::NavStreamPlugin);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open_streamed_level(app.world_mut(), level, LoadMode::Replace).expect("open");
    let source = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(150.0, 0.0, 50.0)),
            StreamingSource {
                kind: SourceKind::Player,
                ..StreamingSource::default()
            },
        ))
        .id();
    let visible = |app: &mut App, cell: CellCoord| {
        for _ in 0..600 {
            app.update();
            if app.world().resource::<WorldStreaming>().state(level, cell) == Some(CellState::Visible) {
                return;
            }
        }
        panic!("{cell:?} never became visible");
    };
    visible(&mut app, CellCoord::new(1, 0));
    let on_mesh = |app: &App, x: f32| {
        app.world()
            .resource::<regia_nav::NavTileCache>()
            .query()
            .find_nearest(Vec3::new(x, 0.0, 50.0), Vec3::splat(1.0))
            .is_some()
    };
    let cache = app.world().resource::<regia_nav::NavTileCache>();
    assert!(cache.is_streamed());
    assert!(!cache.loaded_cells().is_empty());
    assert!(on_mesh(&app, 150.0), "the source stands on its cell's tiles");
    assert!(!on_mesh(&app, 750.0), "a far cell's tiles are not loaded");
    assert_eq!(
        app.world()
            .resource::<regia_nav::NavStreamState>()
            .load_failures(),
        0
    );

    // At the far end the first tiles leave and the last ones arrive.
    app.world_mut()
        .get_mut::<Transform>(source)
        .expect("source")
        .translation = Vec3::new(750.0, 0.0, 50.0);
    visible(&mut app, CellCoord::new(7, 0));
    for _ in 0..30 {
        app.update();
    }
    assert!(on_mesh(&app, 750.0), "tiles arrive with their cell");
    assert!(!on_mesh(&app, 150.0), "tiles leave with their cell");
    let _ = std::fs::remove_dir_all(&dir);
}

/// deep-zai-terrain-stream #1: the stream pass reads the asset rows this cook just wrote, not the
/// manifest the previous cook left on disk. On a fresh cache the cell must already depend on the
/// mesh it names, and the index must equal the one a second cook writes.
#[test]
fn a_fresh_cook_indexes_the_meshes_it_cooked_in_the_same_run() {
    let dir = regia_test_utils::unique_temp_dir("stream_world_fresh_manifest");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("dirs");
    std::fs::create_dir_all(dir.join("assets/props")).expect("dirs");
    std::fs::write(
        dir.join("assets/props/tri.obj"),
        "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n",
    )
    .expect("obj");
    let text = format!(
        "(format_version: 1, entities: [\
         (guid: \"{}\", name: \"prop\", components: {{ \"{TRANSFORM}\": (translation: (50.0, 1.0, 50.0)), \
          \"regia_render::runtime::MeshSource\": (mesh: \"assets/props/tri.obj\", material: \"\") }}),\
         ], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: 100.0, load_radius_m: 40.0, \
         unload_radius_m: 60.0, hlod: None) }})",
        Guid::from_u128(1),
    );
    std::fs::write(dir.join("assets/maps/open.level.ron"), text).expect("write");
    let report = cook_project(CookOptions::for_host(dir.clone())).expect("cook");
    let fatal: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.contains("unsupported source extension"))
        .collect();
    assert!(fatal.is_empty(), "{fatal:?}");
    let meta = std::fs::read(dir.join("assets/maps/open.level.ron.meta")).expect("meta");
    let level = LevelId(regia_asset::MetaSidecar::from_ron(&meta).expect("parse").id.0);
    let cache_dir = cooked_cache_dir(&dir, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache_dir)
        .expect("manifest")
        .expect("present");
    let mesh_ids: Vec<AssetId> = manifest
        .entries
        .iter()
        .filter(|e| e.path == "assets/props/tri.obj" || e.path.starts_with("assets/props/tri.obj#"))
        .map(|e| e.id)
        .collect();
    assert!(!mesh_ids.is_empty(), "the mesh cooked");
    let index_path = cache_dir.join(cooked_file_name(world_index_id(level)));
    let first = std::fs::read(&index_path).expect("index");
    let index = WorldIndex::from_bytes(&first, level).expect("decode");
    assert_eq!(index.cells.len(), 1);
    let cell = &index.cells[0];
    assert!(
        mesh_ids.iter().any(|id| cell.deps.contains(id)),
        "the first cook's cell names the mesh it cooked: deps {:?}, mesh ids {mesh_ids:?}",
        cell.deps
    );
    assert!(cell.dep_bytes > 0, "dep_bytes {}", cell.dep_bytes);

    cook_project(CookOptions::for_host(dir.clone())).expect("cook again");
    assert_eq!(
        std::fs::read(&index_path).expect("index"),
        first,
        "a second cook writes the same index as the first"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
