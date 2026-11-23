// SPDX-License-Identifier: MPL-2.0
//! ADR-8451: `regia cook` cooks the mesh layers of every level that has them, with no extra flag.
//! The level's `terrain` block gives the height mapping of the cooked heightfield, the layer's
//! `material` resolves through the cook's manifest, and the files written are byte for byte the
//! library call's (`cook_mesh_layers`) on the same ground. A missing material is a cook error,
//! never the default rock.
//!
//! Fixture: DO27's Switchbacks passage layout as data (`regia_world_geom`'s `fixtures` feature) on
//! a flat synthetic 1025² heightfield at -256 m; no DO27 map data. CPU only, no GPU.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_asset::{AssetManifest, MetaSidecar, cooked_cache_dir, cooked_file_name, meta_path_for};
use regia_asset_pipeline::terrain_mesh_layers::{
    MeshLayersCookRequest, cook_mesh_layers, material_resolver, stage_a_sampler,
};
use regia_scene::format::SceneFile;
use regia_scene::level::{Level, LevelId, LevelSettings, LevelTerrain};
use regia_terrain::CdlodConfig;
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::cook::GroundInput;
use regia_world_geom::passages::PassagesDocument;

const SIDE: u32 = 1025;
const WIDTH_M: f32 = 1280.0;
/// One count is exactly 1/32 m: `65535 / 32`.
const HEIGHT_SCALE_M: f32 = 65_535.0 / 32.0;
const HEIGHT_OFFSET_M: f32 = -512.0;
/// -512 m + 8192 / 32 m = -256 m, the canyon floor of the W-16 fixtures.
const SAMPLE: u16 = 8192;
const LAYERS: &str = "assets/maps/canyon/terrain.terrain.layers.ron";
const MATERIAL: &str = "materials/rock.material.ron";

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .env("REGIA_LOG", "warn")
        .output()
        .expect("run regia")
}

fn cook(root: &Path) -> Output {
    regia(&[
        "cook",
        "--project",
        root.to_str().expect("utf-8 temp path"),
        "--platform",
        "linux",
        "--quality",
        "fast",
    ])
}

/// A project with a terrain, a level naming it with its height mapping, a `canyon.meshlayers.ron`
/// next to the map (one Switchbacks passage layer drawing `material`) and a standard material.
fn project(tag: &str, material: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    let maps = root.join("assets/maps");
    let ground = maps.join("canyon");
    std::fs::create_dir_all(&ground).unwrap();
    std::fs::create_dir_all(root.join("assets/materials")).unwrap();
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"Canyon\"\nid = \"com.example.canyon\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join(LAYERS),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .unwrap();
    std::fs::write(
        ground.join("heightfield.json"),
        format!("{{\"size_m\":{WIDTH_M}}}"),
    )
    .unwrap();
    let samples = vec![SAMPLE; (SIDE * SIDE) as usize];
    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(SIDE, SIDE, samples)
        .unwrap()
        .save(ground.join("heightfield.png"))
        .unwrap();
    std::fs::write(
        root.join("assets").join(MATERIAL),
        "(\n    format_version: 3,\n    name: \"M_Rock\",\n    shader: Standard,\n    base: (\n        base_color: (0.4, 0.35, 0.3, 1.0),\n        metallic: 0.0,\n        roughness: 0.85,\n    ),\n)\n",
    )
    .unwrap();
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    std::fs::write(maps.join("switchbacks.passages.ron"), passages).unwrap();
    let document = format!(
        "(version: 1, ground_tile_cells: 64, layers: [(id: \"switchbacks\", source: Passages(path: \"switchbacks.passages.ron\"), material: \"{material}\")])"
    );
    std::fs::write(
        maps.join("canyon.meshlayers.ron"),
        waive_known_ramp_facets(&document, &maps),
    )
    .unwrap();
    let level = Level {
        id: LevelId(regia_core::id::Guid(0xc4_17)),
        settings: LevelSettings {
            terrain: Some(LevelTerrain {
                layers: "maps/canyon/terrain.terrain.layers.ron".into(),
                map: "maps/canyon".into(),
                world_width_m: Some(WIDTH_M),
                height_scale_m: HEIGHT_SCALE_M,
                height_offset_m: HEIGHT_OFFSET_M,
                ..Default::default()
            }),
            ..LevelSettings::default()
        },
        scene: SceneFile::new(),
    };
    std::fs::write(maps.join("canyon.level.ron"), level.to_ron_string().unwrap()).unwrap();
    root
}

/// `document` with ADR-8359's K1 waiver rows when its cook on the level's ground is refused only
/// by `ramp_facet_slope_over_limit`: the Switchbacks fixture's two known ramp facets (slope 1.58
/// over the 1.50 limit) refuse every unwaived cook until the Stage B mesher bump, and a test cook
/// authors the hashed waiver in the source document (ADR-8447). `regia cook` never auto-waives,
/// so any other refusal, or a lapsed waiver hash, still fails the cook under test.
fn waive_known_ramp_facets(document: &str, maps: &Path) -> String {
    use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
    use regia_world_geom::cook::{CookError, CookInputs, cook};
    let ground = expected_ground();
    let sampler = stage_a_sampler(&ground).unwrap();
    let read = |p: &str| std::fs::read(maps.join(p)).map_err(|e| e.to_string());
    match cook(&CookInputs {
        document: document.as_bytes(),
        ground: &ground,
        terrain: Some(&sampler),
        read_file: &read,
    }) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            waive_ramp_facets(document, &report).unwrap()
        }
        _ => document.to_string(),
    }
}

/// The ground the level names, written out by hand: the cook must derive exactly this.
fn expected_ground() -> GroundInput {
    GroundInput {
        lattice_n: SIDE,
        width_m: WIDTH_M,
        offset_m: HEIGHT_OFFSET_M,
        step_m: 1.0 / 32.0,
        counts: vec![SAMPLE; (SIDE * SIDE) as usize],
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn regia_cook_writes_each_levels_mesh_layers_byte_for_byte_as_the_library_call() {
    let root = project("cli-meshlayers-cook", MATERIAL);
    let out = cook(&root);
    assert!(out.status.success(), "regia cook failed:\n{}", stderr(&out));

    let cooked_file = root.join("assets/maps/canyon.meshlayers");
    let cli_bytes = std::fs::read(&cooked_file)
        .unwrap_or_else(|e| panic!("regia cook did not write {}: {e}", cooked_file.display()));
    let cache = cooked_cache_dir(&root, "linux");
    let map_id = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&root.join(LAYERS))).unwrap())
        .unwrap()
        .id;
    let manifest = AssetManifest::read_from_cache(&cache).unwrap().unwrap();
    let material_id = manifest
        .id_for_path(&format!("assets/{MATERIAL}"))
        .expect("the material cooked");

    // The library call on the ground the level names, into a separate directory.
    let lib_dir = regia_test_utils::unique_temp_dir("cli-meshlayers-lib");
    let lib_file = lib_dir.join("canyon.meshlayers");
    let lib_cooked = lib_dir.join("cooked");
    let maps = root.join("assets/maps");
    let document = std::fs::read(maps.join("canyon.meshlayers.ron")).unwrap();
    let read = |p: &str| std::fs::read(maps.join(p)).map_err(|e| e.to_string());
    let ground = expected_ground();
    let sampler = stage_a_sampler(&ground).unwrap();
    let resolve = material_resolver(&manifest.entries);
    let lib = cook_mesh_layers(&MeshLayersCookRequest {
        document: &document,
        ground: &ground,
        terrain: Some(&sampler),
        read_file: &read,
        map_id,
        out_file: &lib_file,
        cooked_dir: &lib_cooked,
        cdlod: CdlodConfig::default(),
        origin: [0.0, 0.0],
        resolve_material: &resolve,
    })
    .expect("library cook");
    assert!(lib.cooked);
    assert_eq!(
        cli_bytes,
        std::fs::read(&lib_file).unwrap(),
        "regia cook's <map>.meshlayers is the library call's, byte for byte"
    );

    // Every chunk LOD the presentation names is in the platform cache, byte for byte, with a
    // manifest row (so a pak carries it).
    let layer = &lib.presentation.layers[0];
    assert_eq!(layer.material, MATERIAL);
    assert_eq!(
        layer.material_id,
        Some(material_id),
        "the layer draws its material, not the rock"
    );
    assert!(!layer.chunks.is_empty(), "the layer has chunks");
    let mut lods = 0;
    for c in &layer.chunks {
        for (i, lod) in c.lods.iter().enumerate() {
            let name = cooked_file_name(lod.mesh);
            let cli = std::fs::read(cache.join(&name))
                .unwrap_or_else(|e| panic!("chunk {}_{} lod{i} missing from the cache: {e}", c.x, c.z));
            assert_eq!(
                cli,
                std::fs::read(lib_cooked.join(&name)).unwrap(),
                "chunk {}_{} lod{i}",
                c.x,
                c.z
            );
            let row = format!(
                "assets/maps/canyon.meshlayers.ron#{}",
                regia_terrain::chunk_lod_asset_path(&layer.id, c.x, c.z, i as u8)
            );
            assert_eq!(manifest.id_for_path(&row), Some(lod.mesh), "manifest row {row}");
            lods += 1;
        }
    }
    assert!(lods > layer.chunks.len(), "chunks have LODs past LOD 0");

    // The run-time load reads what the cook wrote.
    let loaded = regia_terrain::load_mesh_layers(&cooked_file, &cache).expect("the cooked layers load");
    assert_eq!(
        loaded.presentation_arc().map(|p| p.layers[0].material_id),
        Some(Some(material_id))
    );

    // The chunks draw the resolved material, not the rock; a layer without one draws the rock,
    // and so (with an error log) does a material id that does not load.
    use regia_terrain::mesh_layer::stream::{layer_chunk_material, layer_material};
    let drawn = layer_material(layer, None, Some(&cache));
    assert_eq!(drawn.base_color, [0.4, 0.35, 0.3, 1.0]);
    assert_eq!(drawn.roughness, 0.85);
    let rock = layer_chunk_material();
    assert_ne!(drawn.base_color, rock.base_color);
    let mut plain = layer.clone();
    plain.material_id = None;
    assert_eq!(
        layer_material(&plain, None, Some(&cache)).base_color,
        rock.base_color
    );
    plain.material_id = Some(map_id);
    assert_eq!(
        layer_material(&plain, None, Some(&cache)).base_color,
        rock.base_color
    );

    // An unchanged second cook writes nothing.
    let before = std::fs::metadata(&cooked_file).unwrap().modified().unwrap();
    let again = cook(&root);
    assert!(again.status.success(), "second cook failed:\n{}", stderr(&again));
    assert_eq!(std::fs::read(&cooked_file).unwrap(), cli_bytes);
    assert_eq!(
        std::fs::metadata(&cooked_file).unwrap().modified().unwrap(),
        before,
        "an unchanged level's mesh layers are not rewritten"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&lib_dir);
}

#[test]
fn a_missing_layer_material_fails_regia_cook_and_writes_no_layers() {
    let root = project("cli-meshlayers-missing", "materials/nowhere.material.ron");
    let out = cook(&root);
    let err = stderr(&out);
    assert!(
        !out.status.success(),
        "a missing material must fail the cook:\n{err}"
    );
    assert!(err.contains("meshlayers_material_missing"), "{err}");
    assert!(err.contains("materials/nowhere.material.ron"), "{err}");
    assert!(
        !root.join("assets/maps/canyon.meshlayers").exists(),
        "no layers cooked with the default rock in its place"
    );
    let _ = std::fs::remove_dir_all(&root);
}
