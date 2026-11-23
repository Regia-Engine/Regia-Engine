// SPDX-License-Identifier: MPL-2.0
//! The terrain a level names (scene format v12, ADR-2411): resolving its files, loading it from the
//! cooked heightfield, the migration of a v11 level to the same terrain, and the ECS plugin.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetId, CookedHeader, MetaSidecar};
use regia_scene::level::{Level, LevelId, LevelSettings, LevelTerrain};
use regia_terrain::{
    CdlodConfig, GrassDensityPaint, GrassDensityStroke, HoleEllipse, HolesMask, LayerWeightMap,
    LevelTerrainFiles, LevelTerrainLoad, LevelTerrainPlugin, LevelTerrainRoots, LevelTerrainSource,
    LevelTerrainStatus, Terrain, TerrainLayers, TerrainMap, TerrainPaint, grass_density_path, holes_ron_path,
    layer_weights_path, load_level_terrain, spawn_level_terrain,
};

const LAYERS: &str = include_str!("fixtures/ground.terrain.layers.ron");
const SIDE: u32 = 65;

struct Project {
    root: PathBuf,
    assets: PathBuf,
    cooked: PathBuf,
    map: TerrainMap,
}

fn heights_map() -> TerrainMap {
    let heights = (0..SIDE * SIDE)
        .map(|i| {
            let (x, z) = ((i % SIDE) as f32, (i / SIDE) as f32);
            10.0 + (x * 0.21).sin() * 4.0 + (z * 0.13).cos() * 3.0
        })
        .collect();
    TerrainMap::from_heights(SIDE, SIDE, heights, 64.0, 40.0, 0.0).expect("map")
}

/// A project with `assets/maps/arena/terrain.layers.ron` (+ `.meta`), its cooked heightfield,
/// and the old level `assets/maps/arena.level.ron` (v11) with its side files under its own name.
fn project(tag: &str) -> Project {
    let root = regia_test_utils::unique_temp_dir(tag);
    let assets = root.join("assets");
    let cooked = root.join("cooked");
    std::fs::create_dir_all(assets.join("maps/arena")).expect("dirs");
    std::fs::create_dir_all(&cooked).expect("dirs");
    let layers = assets.join("maps/arena/terrain.layers.ron");
    std::fs::write(&layers, LAYERS).expect("layers");
    let map_id = AssetId::new_random();
    let meta = MetaSidecar::new("terrain", map_id).to_ron_string().expect("meta");
    std::fs::write(regia_asset::meta_path_for(&layers), meta).expect("meta");
    let map = heights_map();
    let tiled = regia_terrain::cook_all_tiles(&map, 32).expect("cook");
    let id = regia_terrain::stream::heightfield_asset_id(map_id);
    let payload = postcard::to_allocvec(&tiled).expect("postcard");
    let header = CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(regia_terrain::stream::TILED_HEIGHTFIELD_TYPE_PATH),
        asset_id: id,
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = regia_asset::encode_cooked_file(&header, &payload).expect("encode");
    std::fs::write(cooked.join(regia_asset::cooked_file_name(id)), file).expect("cooked");

    let level = assets.join("maps/arena.level.ron");
    std::fs::write(
        &level,
        r#"SceneFile(
    format_version: 11,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.25)),
    },
)"#,
    )
    .expect("level");
    let holes = HolesMask::new(vec![HoleEllipse {
        center: [3.0, -2.0],
        radius: [2.0, 1.5],
        rotation_radians: 0.0,
    }])
    .expect("holes");
    holes.validate().expect("valid");
    std::fs::write(holes_ron_path(&level), holes.to_holes_ron().expect("ron")).expect("holes");
    let mut weights = LayerWeightMap::new(SIDE, SIDE, 2).expect("weights");
    if let Some(mut layer) = weights.layer_mut(1) {
        use regia_terrain::brush::StrokeTarget;
        layer.write(glam::IVec2::new(4, 4), 200);
    }
    std::fs::write(layer_weights_path(&level), weights.to_bytes().expect("bytes")).expect("weights");
    let mut density = GrassDensityPaint::new(32).expect("density");
    density
        .stroke(
            &GrassDensityStroke {
                path: vec![[0.0, 0.0], [10.0, 6.0]],
                radius_m: 5.0,
                hardness: 0.5,
                strength: 1.0,
                target: 0.0,
            },
            64.0,
        )
        .expect("stroke");
    std::fs::write(grass_density_path(&level), density.to_bytes().expect("bytes")).expect("density");
    Project {
        root,
        assets,
        cooked,
        map,
    }
}

fn arena_terrain() -> LevelTerrain {
    LevelTerrain {
        layers: "maps/arena/terrain.layers.ron".into(),
        map: String::new(),
        world_width_m: Some(64.0),
        height_scale_m: 40.0,
        height_offset_m: 0.0,
        ..Default::default()
    }
}

/// What a host did before v12: built the terrain itself from the cooked heightfield and read the
/// side files by the level's own name.
fn host_load(p: &Project, level: &Path) -> LevelTerrainLoad {
    let mut terrain = Terrain::new(
        &p.map.to_heightfield(),
        64.0,
        40.0,
        0.0,
        CdlodConfig::default(),
        1,
        TerrainLayers::from_ron(LAYERS).expect("layers"),
    )
    .expect("terrain");
    let holes = regia_terrain::load_holes_ron(&holes_ron_path(level)).expect("holes");
    terrain.holes.clone_from(&holes);
    LevelTerrainLoad {
        files: LevelTerrainFiles::resolve(&p.assets, "maps/arena.level.ron", None).expect("files"),
        terrain: Some(terrain),
        map_id: None,
        cooked_heightfield: None,
        holes,
        paint: Some(
            LayerWeightMap::from_bytes(&std::fs::read(layer_weights_path(level)).expect("read"))
                .expect("weights"),
        ),
        grass_density: Some(
            GrassDensityPaint::from_bytes(&std::fs::read(grass_density_path(level)).expect("read"))
                .expect("density"),
        ),
        source_mapping: None,
        mesh_layers: None,
    }
}

/// ADR-2411 acceptance: an old level migrates and loads the same terrain. The v11 level migrates
/// naming no terrain, so its side files resolve to the names the pre-v12 editor used; named with
/// the terrain it stood on (an empty `map` keeps the level's name), the loader builds a terrain
/// whose digest equals the host-built one.
#[test]
fn an_old_level_migrates_and_loads_the_same_terrain() {
    let p = project("level_terrain_migrate");
    let level_path = p.assets.join("maps/arena.level.ron");
    let text = std::fs::read_to_string(&level_path).expect("read");
    let level = Level::from_ron_str(LevelId::default(), &text).expect("the v11 level loads");
    assert_eq!(
        level.scene.format_version,
        regia_scene::format::SCENE_FORMAT_VERSION
    );
    assert_eq!(level.settings.terrain, None, "a v11 level names no terrain");

    let files =
        LevelTerrainFiles::resolve(&p.assets, "maps/arena.level.ron", level.settings.terrain.as_ref())
            .expect("files");
    assert_eq!(files.holes, holes_ron_path(&level_path));
    assert_eq!(files.weights, layer_weights_path(&level_path));
    assert_eq!(files.grass_density, grass_density_path(&level_path));
    assert_eq!(files.layers, None);
    let side_only = load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", None).expect("loads");
    assert!(
        side_only.terrain.is_none(),
        "the host still supplies the heightfield"
    );
    assert!(side_only.holes.is_some() && side_only.paint.is_some() && side_only.grass_density.is_some());

    // The level re-saved naming its terrain (what `terrain.set_level_terrain` writes).
    let mut named = level.clone();
    named.settings.terrain = Some(arena_terrain());
    let saved = named.to_ron_string().expect("saves");
    let reread = Level::from_ron_str(LevelId::default(), &saved).expect("reads");
    let load = load_level_terrain(
        &p.assets,
        &p.cooked,
        "maps/arena.level.ron",
        reread.settings.terrain.as_ref(),
    )
    .expect("loads");
    let host = host_load(&p, &level_path);
    assert_eq!(load.digest(), host.digest(), "the same terrain");
    let terrain = load.terrain.as_ref().expect("terrain");
    assert_eq!(
        terrain.map.to_heightfield().samples,
        p.map.to_heightfield().samples
    );
    assert!(terrain.holes.is_some(), "the holes file is applied");
    assert!(load.map_id.is_some() && load.cooked_heightfield.is_some());

    // A different heightfield is a different digest.
    let mut other = host_load(&p, &level_path);
    let mut hf = p.map.to_heightfield();
    hf.samples[3 * SIDE as usize + 3] ^= 0x0100;
    other.terrain = Some(
        Terrain::new(
            &hf,
            64.0,
            40.0,
            0.0,
            CdlodConfig::default(),
            1,
            TerrainLayers::from_ron(LAYERS).expect("layers"),
        )
        .expect("terrain"),
    );
    assert_ne!(other.digest(), load.digest());
    let _ = std::fs::remove_dir_all(&p.root);
}

/// The digest the agent publishes as the loaded level's identity is pinned: it mixes the
/// heightfield, the holes (as their canonical RON), the painted weights and the grass density,
/// so an accidental change to any input or to the serialisation is caught here
/// (audit sprint-audit-g-terrain3-determinism).
#[test]
fn level_digest_is_pinned() {
    let p = project("level_digest_pin");
    let load = load_level_terrain(
        &p.assets,
        &p.cooked,
        "maps/arena.level.ron",
        Some(&arena_terrain()),
    )
    .expect("loads");
    assert_eq!(
        load.digest(),
        LEVEL_DIGEST_PIN,
        "the level digest changed; if intentional, update the pin with the reason"
    );
    let _ = std::fs::remove_dir_all(&p.root);
}

/// The exact digest of the fixture project above (pinned from the LF-serialised post-fix
/// output; identical on every host).
const LEVEL_DIGEST_PIN: u64 = 451_432_381_928_073_742;

/// A level naming a shared `map` reads the side files under that name, not its own.
#[test]
fn a_named_map_moves_the_side_files() {
    let p = project("level_terrain_map");
    let block = LevelTerrain {
        map: "maps/arena/arena".into(),
        ..arena_terrain()
    };
    let files = LevelTerrainFiles::resolve(&p.assets, "maps/arena.level.ron", Some(&block)).expect("files");
    assert_eq!(files.holes, p.assets.join("maps/arena/arena.holes.ron"));
    assert_eq!(files.weights, p.assets.join("maps/arena/arena.weights.bin"));
    assert_eq!(
        files.grass_density,
        p.assets.join("maps/arena/arena.grassdensity.bin")
    );
    let load = load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", Some(&block)).expect("loads");
    assert!(load.terrain.is_some());
    assert!(
        load.holes.is_none() && load.paint.is_none() && load.grass_density.is_none(),
        "no files there"
    );
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn load_errors_carry_their_codes() {
    let p = project("level_terrain_errors");
    let bad = LevelTerrain {
        layers: "../escape.terrain.layers.ron".into(),
        ..arena_terrain()
    };
    let err =
        load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", Some(&bad)).expect_err("refused");
    assert_eq!(err.code(), "level_terrain_invalid");
    let missing = LevelTerrain {
        layers: "maps/none/terrain.layers.ron".into(),
        ..arena_terrain()
    };
    let err = load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", Some(&missing))
        .expect_err("missing");
    assert_eq!(err.code(), "level_terrain_read");
    std::fs::write(
        layer_weights_path(&p.assets.join("maps/arena.level.ron")),
        b"junk",
    )
    .expect("write");
    let err = load_level_terrain(
        &p.assets,
        &p.cooked,
        "maps/arena.level.ron",
        Some(&arena_terrain()),
    )
    .expect_err("broken");
    assert_eq!(err.code(), "level_terrain_decode");
    let err = load_level_terrain(&p.assets, &p.cooked, "/abs.level.ron", None).expect_err("absolute level");
    assert_eq!(err.code(), "level_terrain_invalid");
    // A `.` part would name files beside assets/ (`assets.holes.ron`): refused.
    let dot = LevelTerrain {
        map: ".".into(),
        ..arena_terrain()
    };
    let err = load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", Some(&dot)).expect_err("dot");
    assert_eq!(err.code(), "level_terrain_invalid");
    #[cfg(unix)]
    {
        // A symlink under assets/ cannot take the side files outside it.
        let outside = p.root.join("outside");
        std::fs::create_dir_all(&outside).expect("dir");
        std::os::unix::fs::symlink(&outside, p.assets.join("maps/link")).expect("symlink");
        let linked = LevelTerrain {
            map: "maps/link/arena".into(),
            ..arena_terrain()
        };
        let err = load_level_terrain(&p.assets, &p.cooked, "maps/arena.level.ron", Some(&linked))
            .expect_err("escape");
        assert_eq!(err.code(), "level_terrain_invalid");
    }
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn spawn_replaces_the_previous_level_terrain_and_the_plugin_loads_on_change() {
    let p = project("level_terrain_spawn");
    let roots = LevelTerrainRoots {
        assets: p.assets.clone(),
        cooked: p.cooked.clone(),
        level: "maps/arena.level.ron".into(),
    };
    let settings = LevelSettings {
        terrain: Some(arena_terrain()),
        ..LevelSettings::default()
    };
    let mut world = World::new();
    let first = spawn_level_terrain(&mut world, &roots, &settings).expect("spawn");
    let e = first.entity.expect("entity");
    assert!(world.get::<Terrain>(e).is_some());
    assert!(world.get::<TerrainPaint>(e).is_some());
    assert!(world.get::<GrassDensityPaint>(e).is_some());
    let again = spawn_level_terrain(&mut world, &roots, &settings).expect("spawn");
    assert_eq!(again.replaced, 1);
    assert_eq!(again.digest, first.digest);
    assert!(world.get_entity(e).is_err(), "the earlier entity is gone");
    // A level that names no heightfield leaves the world alone.
    let none = spawn_level_terrain(&mut world, &roots, &LevelSettings::default()).expect("spawn");
    assert_eq!((none.entity, none.replaced), (None, 0));
    assert_eq!(world.query::<&LevelTerrainSource>().iter(&world).count(), 1);
    // Opening another level that names none removes the earlier level's terrain.
    let other = LevelTerrainRoots {
        level: "maps/other.level.ron".into(),
        ..roots.clone()
    };
    let switched = spawn_level_terrain(&mut world, &other, &LevelSettings::default()).expect("spawn");
    assert_eq!((switched.entity, switched.replaced), (None, 1));
    assert_eq!(world.query::<&LevelTerrainSource>().iter(&world).count(), 0);

    let mut app = App::new();
    app.add_plugins(LevelTerrainPlugin);
    app.insert_resource(roots);
    app.insert_resource(settings);
    app.update();
    let status = app.world().resource::<LevelTerrainStatus>().clone();
    assert_eq!(status.error, None);
    assert_eq!(status.digest, Some(first.digest));
    app.update();
    let count = app
        .world_mut()
        .query::<&LevelTerrainSource>()
        .iter(app.world())
        .count();
    assert_eq!(count, 1, "an unchanged level loads once");
    let _ = std::fs::remove_dir_all(&p.root);
}

/// Plan row W-16 (spec 26 § 2a.5): a map without `<map>.meshlayers` loads no layers. The terrain
/// entity has no `TerrainMeshLayers`, and the layer stream, run with a view on the map, spawns no
/// chunk entity.
#[test]
fn no_meshlayers_file_no_entities() {
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::StreamViews;
    use regia_terrain::{TerrainLayerChunk, TerrainLayerStream, TerrainMeshLayers};

    let p = project("level_terrain_no_meshlayers");
    let roots = LevelTerrainRoots {
        assets: p.assets.clone(),
        cooked: p.cooked.clone(),
        level: "maps/arena.level.ron".into(),
    };
    let settings = LevelSettings {
        terrain: Some(arena_terrain()),
        ..LevelSettings::default()
    };
    let files = LevelTerrainFiles::resolve(&p.assets, "maps/arena.level.ron", settings.terrain.as_ref())
        .expect("files");
    assert_eq!(files.meshlayers, p.assets.join("maps/arena.meshlayers"));
    assert!(!files.meshlayers.exists());
    let mut app = App::new();
    app.insert_resource(TerrainLayerStream::new(f32::INFINITY, u64::MAX));
    app.insert_resource(StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [0.0, 0.0, 0.0],
    )]));
    app.add_systems(Update, regia_terrain::mesh_layer::stream::drive_layer_stream);
    let spawn = spawn_level_terrain(app.world_mut(), &roots, &settings).expect("spawn");
    let e = spawn.entity.expect("entity");
    assert!(app.world().get::<TerrainMeshLayers>(e).is_none());
    app.update();
    app.update();
    let chunks = app
        .world_mut()
        .query::<&TerrainLayerChunk>()
        .iter(app.world())
        .count();
    assert_eq!(chunks, 0);
    let stats = app.world().resource::<TerrainLayerStream>().stats();
    assert_eq!((stats.loads, stats.drawn, stats.bytes), (0, 0, 0));

    // A file that does not load is logged and gives no layers; the load itself succeeds and its
    // digest is unchanged.
    std::fs::write(&files.meshlayers, b"RGML not a cooked file").expect("write");
    let bad = spawn_level_terrain(app.world_mut(), &roots, &settings)
        .expect("a bad meshlayers file is not an error");
    assert_eq!(bad.digest, spawn.digest);
    assert!(
        app.world()
            .get::<TerrainMeshLayers>(bad.entity.expect("entity"))
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&p.root);
}

/// ADR-8816 (cooked mesh layers v2): `load_mesh_layers` runs the count-body check beside the load.
/// A good file gives the geometry and `content_hash` of `WorldGeom::from_cooked`; a file with one
/// changed ground count gives no layers, as the checked load refuses it (`geom_corrupt`).
#[test]
fn mesh_layers_load_checks_the_count_body() {
    use regia_world_geom::WorldGeom;
    use regia_world_geom::cook::{CookInputs, GroundInput, cook};

    let n = 385u32;
    let ground = GroundInput {
        lattice_n: n,
        width_m: (n - 1) as f32,
        offset_m: 0.0,
        step_m: 1.0 / 32.0,
        counts: (0..n * n).map(|i| (i * 7 % 60_000) as u16).collect(),
    };
    let file = cook(&CookInputs {
        document: b"(version:1, ground_tile_cells:16, layers:[])",
        ground: &ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .expect("a ground-only cook")
    .file;
    let bytes = file.encode().expect("encode");
    let dir = regia_test_utils::unique_temp_dir("mesh_layers_body_check");
    std::fs::create_dir_all(&dir).expect("create the folder");
    let path = dir.join("map.meshlayers");
    std::fs::write(&path, &bytes).expect("write");
    let layers = regia_terrain::load_mesh_layers(&path, &dir).expect("a good file loads");
    let want = WorldGeom::from_cooked(&bytes).expect("the checked load");
    let geom = layers.geom().expect("the layers carry the geometry");
    assert_eq!(geom.content_hash(), want.content_hash());
    assert_eq!(geom.cooked(), want.cooked());

    // The last byte of the canonical section is a ground count's high byte.
    let parts = regia_world_geom::cooked::split_file_ref(&bytes).expect("split");
    let at = parts.canonical.as_ptr() as usize - bytes.as_ptr() as usize + parts.canonical.len() - 1;
    let mut bad = bytes.clone();
    bad[at] ^= 1;
    assert_eq!(
        WorldGeom::from_cooked(&bad).map_err(|e| e.code()).err(),
        Some("geom_corrupt")
    );
    std::fs::write(&path, &bad).expect("write");
    assert!(
        regia_terrain::load_mesh_layers(&path, &dir).is_none(),
        "a changed count loaded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-10079 (W-24): a level whose terrain block says `sample_layout: CellCentered` loads a map whose
/// heights sit at the centers of `n` equal cells across `world_width_m`: sample `i` of 65 across
/// 64 m at `(i + 0.5) · 64 / 65 − 32`, and the bilinear surface returns the cooked sample there.
/// Written in level text with only base-era API, so it builds before the change; there the field is
/// ignored and sample 0 sits on the edge (−32 m), so it fails.
#[test]
fn a_cell_centered_level_loads_its_heights_at_cell_centers() {
    let p = project("level_terrain_cell_centered");
    let text = format!(
        r#"SceneFile(
    format_version: {},
    entities: [],
    resources: {{
        "regia_scene::level::LevelSettings": (terrain: Some((
            layers: "maps/arena/terrain.layers.ron",
            map: "",
            world_width_m: Some(64.0),
            height_scale_m: 40.0,
            height_offset_m: 0.0,
            sample_layout: CellCentered,
        ))),
    }},
)"#,
        regia_scene::format::SCENE_FORMAT_VERSION
    );
    let level = Level::from_ron_str(LevelId::default(), &text).expect("the level loads");
    let load = load_level_terrain(
        &p.assets,
        &p.cooked,
        "maps/arena.level.ron",
        level.settings.terrain.as_ref(),
    )
    .expect("loads");
    let map = &load.terrain.as_ref().expect("a terrain").map;
    let cell = 64.0f64 / f64::from(SIDE);
    assert!(
        (f64::from(map.sample_spacing()) - cell).abs() < 1e-6,
        "spacing {} is W / n",
        map.sample_spacing()
    );
    let host = p.map.to_heightfield();
    let host = TerrainMap::new(&host, 64.0, 40.0, 0.0).expect("host map");
    for (col, row) in [
        (0u32, 0u32),
        (1, 0),
        (0, 64),
        (17, 40),
        (32, 32),
        (64, 64),
        (63, 1),
    ] {
        let x = ((f64::from(col) + 0.5) * cell - 32.0) as f32;
        let z = ((f64::from(row) + 0.5) * cell - 32.0) as f32;
        let [px, pz] = map.sample_to_world(col, row);
        assert!(
            (px - x).abs() < 1e-4 && (pz - z).abs() < 1e-4,
            "sample ({col}, {row}) at ({px}, {pz}), its cell center is ({x}, {z})"
        );
        let stored = host.height_at_sample(i64::from(col), i64::from(row));
        assert_eq!(map.height_at_sample(i64::from(col), i64::from(row)), stored);
        let got = map.sample_bilinear(x, z);
        assert!(
            (got - stored).abs() < 1e-3,
            "({col}, {row}): {got} at the cell center, the sample is {stored}"
        );
    }
}
