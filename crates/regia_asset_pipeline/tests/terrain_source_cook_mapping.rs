// SPDX-License-Identifier: MPL-2.0
//! ADR-8471 (CT-19): a count source's datum, scale, cell and registration travel from a schema-2
//! `heightfield.json` through `regia cook`, the cooked terrain index and the level load, beside the
//! verbatim tiled counts. CPU only; no GPU.
//!
//! The fixtures are original: noncentered grids with a nonzero datum, built here.

use std::path::Path;

use regia_asset::{cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::terrain_cook::{
    TERRAIN_INPUTS_HASH_DOMAIN, canonical_extents_bytes, terrain_settings_hash,
};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_scene::level::LevelTerrain;
use regia_terrain::source::{CountMapping, ImportLimits, TerrainSource};
use regia_terrain::stream::{
    TERRAIN_INDEX_FORMAT_VERSION, TerrainIndex, decode_cooked_tiled_heightfield, heightfield_asset_id,
    load_terrain_index_bytes, stitch_tiled_heightfield, terrain_index_asset_id,
};

const DIR: &str = "assets/maps/ridge";
const LAYER: &str = "assets/maps/ridge/ridge.terrain.layers.ron";
const LAYER_REL: &str = "maps/ridge/ridge.terrain.layers.ron";
const LEVEL: &str = "maps/ridge.level.ron";
const PLATFORM: &str = "linux";

/// A ridge with a nonzero datum: counts around 20 000, some under the datum.
fn counts(cols: u32, rows: u32) -> Vec<u16> {
    (0..rows)
        .flat_map(|r| (0..cols).map(move |c| 19_870 + ((c * 37 + r * 91) % 263) as u16))
        .collect()
}

fn mapping_json(cols: u32, rows: u32, mpc: &str, zero: u16, cell: &str, x0: &str, z0: &str) -> String {
    format!(
        r#"{{"schema": 2,
  "grid": {{"cols": {cols}, "rows": {rows}, "cell_m": {cell}, "sample0_x_m": {x0}, "sample0_z_m": {z0},
            "row0": "south", "col0": "west"}},
  "units": {{"kind": "counts", "meters_per_count": {mpc}, "zero_counts": {zero}}},
  "note": "original CT-19 fixture"}}"#
    )
}

fn write_project(root: &Path, cols: u32, rows: u32, json: &str) {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(
        root.join(LAYER),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .expect("layers");
    let image = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(cols, rows, counts(cols, rows))
        .expect("height image");
    image.save(dir.join("heightfield.png")).expect("height png");
    std::fs::write(dir.join("heightfield.json"), json).expect("heightfield.json");
}

fn cook(root: &Path, incremental: bool) -> regia_asset_pipeline::CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: PLATFORM.into(),
        incremental,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn map_id(root: &Path) -> regia_asset::AssetId {
    let meta = std::fs::read(regia_asset::meta_path_for(&root.join(LAYER))).expect("layer meta");
    regia_asset::MetaSidecar::from_ron(&meta).expect("meta").id
}

fn index_bytes(root: &Path) -> Vec<u8> {
    let cache = cooked_cache_dir(root, PLATFORM);
    std::fs::read(cache.join(cooked_file_name(terrain_index_asset_id(map_id(root))))).expect("terrain index")
}

fn index(root: &Path) -> TerrainIndex {
    load_terrain_index_bytes(&index_bytes(root)).expect("index decodes")
}

/// The cooked source: tiled counts stitched back, under the index's mapping.
fn cooked_source(root: &Path) -> TerrainSource {
    let cache = cooked_cache_dir(root, PLATFORM);
    let bytes =
        std::fs::read(cache.join(cooked_file_name(heightfield_asset_id(map_id(root))))).expect("tiled");
    let tiled = decode_cooked_tiled_heightfield(&bytes).expect("tiled decodes");
    let hf = stitch_tiled_heightfield(&tiled).expect("stitch");
    let mapping = index(root).source_mapping.expect("the index carries the mapping");
    TerrainSource::from_counts(hf.samples, mapping, ImportLimits::default()).expect("cooked source")
}

fn block() -> LevelTerrain {
    LevelTerrain {
        layers: LAYER_REL.into(),
        // A count source ignores these; a legacy terrain uses them.
        height_scale_m: 40.0,
        height_offset_m: -10.0,
        ..LevelTerrain::default()
    }
}

fn bits(v: [f32; 2]) -> [u32; 2] {
    [v[0].to_bits(), v[1].to_bits()]
}

/// A square, noncentered grid with a nonzero datum: the cook persists the exact mapping, the
/// cooked counts and meters match the source bit for bit, the level loads it with every sample at
/// the source's world position, and source height, runtime sample and collider agree at the
/// corners and interior control points. A recook writes the same index.
#[test]
fn a_noncentered_count_source_keeps_counts_meters_and_positions_through_cook_load_and_recook() {
    let root = regia_test_utils::unique_temp_dir("terrain-mapping-square");
    let (n, cell, x0) = (33u32, 0.5f32, 37.25f32);
    let json = mapping_json(n, n, "0.03125", 20_000, "0.5", "37.25", "37.25");
    write_project(&root, n, n, &json);
    // The source as the importer reads it: the same counts under the same mapping.
    let png = std::fs::read(root.join(DIR).join("heightfield.png")).expect("png");
    let expected = CountMapping {
        cols: n,
        rows: n,
        meters_per_count: 0.03125,
        zero_counts: 20_000,
        cell_m: cell,
        sample0_m: [x0, x0],
    };
    let source = TerrainSource::from_png(
        &png,
        &regia_terrain::source::PngImportSettings {
            meters_per_count: 0.03125,
            cell_m: cell,
            sample0: Some([x0, x0]),
            missing_datum: regia_terrain::source::MissingDatum::Counts(20_000),
            ..Default::default()
        },
        ImportLimits::default(),
    )
    .expect("png source");
    assert_eq!(source.mapping(), expected);

    cook(&root, false);
    let idx = index(&root);
    assert_eq!(idx.version, TERRAIN_INDEX_FORMAT_VERSION);
    let persisted = idx.source_mapping.expect("mapping persisted");
    assert_eq!(persisted.canonical_bytes(), expected.canonical_bytes());
    assert_eq!(idx.world_size_m, None);

    let cooked = cooked_source(&root);
    assert_eq!(cooked.counts(), source.counts(), "counts verbatim");
    let meter_bits = |s: &TerrainSource| s.heights_m().iter().map(|h| h.to_bits()).collect::<Vec<_>>();
    assert_eq!(meter_bits(&cooked), meter_bits(&source), "meters bit for bit");
    assert_eq!(
        bits(cooked.sample_world(n - 1, n - 1)),
        bits([x0 + 16.0, x0 + 16.0])
    );

    let loaded = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block()),
    )
    .expect("the level loads the count source");
    assert_eq!(loaded.source_mapping, Some(persisted));
    let terrain = loaded.terrain.expect("terrain");
    let map = &terrain.map;
    assert_eq!(
        map.grid_origin(),
        [45.25, 45.25],
        "center of the noncentered grid"
    );
    assert_eq!(
        map.to_heightfield().samples,
        source.counts(),
        "an export gives the counts back"
    );
    let collider = map.collision_heightfield(None);
    let controls = [
        (0, 0),
        (n - 1, 0),
        (0, n - 1),
        (n - 1, n - 1),
        (7, 19),
        (16, 16),
        (31, 2),
    ];
    for (col, row) in controls {
        let [x, z] = source.sample_world(col, row);
        assert_eq!(
            bits(map.sample_to_world(col, row)),
            bits([x, z]),
            "sample ({col}, {row})"
        );
        let want = source.height_m(col, row);
        assert_eq!(
            map.height_at_sample(i64::from(col), i64::from(row)).to_bits(),
            want.to_bits(),
            "stored height ({col}, {row})"
        );
        assert_eq!(
            map.sample_bilinear(x, z).to_bits(),
            want.to_bits(),
            "runtime sample ({col}, {row})"
        );
        let hit = collider.height_at(x, z).expect("inside the collider");
        assert!(
            (hit - want).abs() <= 1e-3,
            "collider ({col}, {row}): {hit} vs {want}"
        );
    }
    // Between samples the collider stays within its documented 1 mm of the runtime surface.
    for (col, row) in [(3u32, 4u32), (20, 11), (30, 30)] {
        let [x, z] = source.sample_world(col, row);
        let (x, z) = (x + cell * 0.5, z + cell * 0.25);
        let surface = map.sample_bilinear(x, z);
        let hit = collider.height_at(x, z).expect("inside the collider");
        assert!((hit - surface).abs() <= collider.max_bilinear_delta() + 1e-3);
    }
    // Outside the source's world bounds the collider has no ground.
    assert!(collider.height_at(x0 - 1.0, x0 + 1.0).is_none());

    // Recook: an unchanged incremental cook skips, a full cook writes the same index bytes.
    let before = index_bytes(&root);
    let again = cook(&root, true);
    assert!(again.skipped >= 1, "{again:?}");
    assert_eq!(index_bytes(&root), before);
    cook(&root, false);
    assert_eq!(index_bytes(&root), before, "a recook is deterministic");
    assert_eq!(meter_bits(&cooked_source(&root)), meter_bits(&source));
    let _ = std::fs::remove_dir_all(&root);
}

/// A rectangular, noncentered grid keeps its exact mapping and counts through the cook and back;
/// the runtime map (square in world space) refuses it with `level_terrain_mapping` instead of
/// resampling or stretching it.
#[test]
fn a_rectangular_source_round_trips_its_mapping_and_the_runtime_refuses_rather_than_resamples() {
    let root = regia_test_utils::unique_temp_dir("terrain-mapping-rect");
    let (cols, rows) = (9u32, 5u32);
    write_project(
        &root,
        cols,
        rows,
        &mapping_json(cols, rows, "0.125", 19_950, "2.5", "-101.5", "640.0"),
    );
    cook(&root, false);
    let mapping = index(&root).source_mapping.expect("mapping");
    assert_eq!((mapping.cols, mapping.rows), (cols, rows));
    assert_eq!(bits(mapping.world_min()), bits([-101.5, 640.0]));
    assert_eq!(bits(mapping.world_max()), bits([-81.5, 650.0]));
    let cooked = cooked_source(&root);
    assert_eq!(cooked.counts(), counts(cols, rows).as_slice());
    let first = cooked.height_m(0, 0);
    assert_eq!(
        first.to_bits(),
        ((19_870.0f32 - 19_950.0) * 0.125).to_bits(),
        "below the datum"
    );
    let err = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block()),
    )
    .expect_err("a rectangular grid is not loaded into the square runtime map");
    assert_eq!(err.code(), "level_terrain_mapping");
    assert!(err.to_string().contains("nothing was resampled"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Changing only the datum, the cell size or the registration changes the terrain's input hash,
/// so an incremental cook recooks and the level reads the new mapping.
#[test]
fn changing_only_datum_cell_or_origin_invalidates_the_cache() {
    let root = regia_test_utils::unique_temp_dir("terrain-mapping-invalidate");
    let n = 17u32;
    let base = mapping_json(n, n, "0.0625", 20_000, "1.0", "12.0", "12.0");
    write_project(&root, n, n, &base);
    let layer = root.join(LAYER);
    cook(&root, false);
    let first = terrain_settings_hash(&layer).expect("hash");
    let variants = [
        (
            "datum",
            mapping_json(n, n, "0.0625", 20_001, "1.0", "12.0", "12.0"),
        ),
        (
            "cell",
            mapping_json(n, n, "0.0625", 20_000, "2.0", "12.0", "12.0"),
        ),
        (
            "origin",
            mapping_json(n, n, "0.0625", 20_000, "1.0", "12.5", "12.5"),
        ),
        (
            "scale",
            mapping_json(n, n, "0.125", 20_000, "1.0", "12.0", "12.0"),
        ),
    ];
    let mut seen = vec![first];
    for (what, json) in &variants {
        std::fs::write(root.join(DIR).join("heightfield.json"), json).expect("rewrite");
        let hash = terrain_settings_hash(&layer).expect("hash");
        assert!(!seen.contains(&hash), "{what}: the hash must change");
        seen.push(hash);
        let report = cook(&root, true);
        assert_eq!(
            report.skipped, 0,
            "{what}: the incremental cook recooks the terrain"
        );
        let mapping = index(&root).source_mapping.expect("mapping");
        let parsed = CountMapping::from_heightfield_json(json.as_bytes())
            .expect("parse")
            .expect("mapping");
        assert_eq!(mapping.canonical_bytes(), parsed.canonical_bytes(), "{what}");
    }
    // The same mapping spelled differently hashes alike (resolved, not textual).
    std::fs::write(root.join(DIR).join("heightfield.json"), &base).expect("rewrite");
    let spelled = base
        .replace("\"cell_m\": 1.0", "\"cell_m\": 1e0")
        .replace("\"note\": \"original CT-19 fixture\"", "\"note\": \"x\"");
    let a = canonical_extents_bytes(base.as_bytes());
    let b = canonical_extents_bytes(spelled.as_bytes());
    assert_eq!(a, b);
    assert_eq!(terrain_settings_hash(&layer).expect("hash"), first);
    let _ = std::fs::remove_dir_all(&root);
}

/// A legacy extents file keeps every existing behavior: the same canonical bytes and hash as
/// before ADR-8471, an index with no mapping, and the level's scale, offset and the index width.
#[test]
fn legacy_extents_files_keep_their_hash_index_and_runtime_mapping() {
    let legacy = br#"{"size_m": 64, "origin": [0, -5.0, 0], "grid": {"anything": true}}"#;
    // The pre-ADR-8471 layout, by hand: has_size | size | has_origin | origin.
    let mut expected = vec![1u8];
    expected.extend_from_slice(&64.0f32.to_bits().to_le_bytes());
    expected.push(1);
    for v in [0.0f32, -5.0, 0.0] {
        expected.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    assert_eq!(canonical_extents_bytes(legacy), expected);
    assert_eq!(
        TERRAIN_INPUTS_HASH_DOMAIN, b"regia-terrain-inputs-v2\0",
        "the domain is unchanged"
    );

    let root = regia_test_utils::unique_temp_dir("terrain-mapping-legacy");
    let n = 17u32;
    write_project(&root, n, n, r#"{"size_m": 64.0}"#);
    cook(&root, false);
    let idx = index(&root);
    assert_eq!(idx.source_mapping, None);
    assert_eq!(idx.world_size_m, Some(64.0));
    let loaded = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block()),
    )
    .expect("legacy load");
    assert_eq!(loaded.source_mapping, None);
    let terrain = loaded.terrain.expect("terrain");
    assert_eq!(terrain.map.world_width(), 64.0);
    assert_eq!(terrain.map.height_scale(), 40.0);
    assert_eq!(terrain.map.height_offset(), -10.0);
    assert_eq!(terrain.map.grid_origin(), [0.0, 0.0]);
    let s = counts(n, n)[0];
    assert_eq!(
        terrain.map.height_at_sample(0, 0).to_bits(),
        (f32::from(s) / 65535.0 * 40.0 + -10.0).to_bits(),
        "the legacy dequantisation, unchanged"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A mapping that is not the heightfield's grid, or that gives the registration twice, fails the
/// cook instead of loading other positions.
#[test]
fn a_mapping_for_another_grid_or_with_two_registrations_fails_the_cook() {
    for (name, json) in [
        (
            "grid",
            mapping_json(16, 17, "0.0625", 20_000, "1.0", "0.0", "0.0"),
        ),
        (
            "twice",
            r#"{"schema": 2, "size_m": 16, "grid": {"cols": 17, "rows": 17, "cell_m": 1},
                "units": {"kind": "counts", "meters_per_count": 1, "zero_counts": 0}}"#
                .to_owned(),
        ),
    ] {
        let root = regia_test_utils::unique_temp_dir(&format!("terrain-mapping-bad-{name}"));
        write_project(&root, 17, 17, &json);
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: PLATFORM.into(),
            incremental: false,
        });
        let failed = match report {
            Err(_) => true,
            Ok(r) => !r.errors.is_empty(),
        };
        assert!(failed, "{name}: the cook refuses the mapping");
        if regia_asset::meta_path_for(&root.join(LAYER)).is_file() {
            assert!(
                !cooked_cache_dir(&root, PLATFORM)
                    .join(cooked_file_name(terrain_index_asset_id(map_id(&root))))
                    .is_file(),
                "{name}: no index is written"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Changing a level's explicit world width away from the mapping's is refused, not applied.
#[test]
fn a_level_width_that_contradicts_the_mapping_is_refused() {
    let root = regia_test_utils::unique_temp_dir("terrain-mapping-width");
    let n = 17u32;
    write_project(
        &root,
        n,
        n,
        &mapping_json(n, n, "0.0625", 20_000, "1.0", "-3.0", "-3.0"),
    );
    cook(&root, false);
    let cache = cooked_cache_dir(&root, PLATFORM);
    let wrong = LevelTerrain {
        world_width_m: Some(64.0),
        ..block()
    };
    let err = regia_terrain::load_level_terrain(&root.join("assets"), &cache, LEVEL, Some(&wrong))
        .expect_err("contradiction");
    assert_eq!(err.code(), "level_terrain_mapping");
    let right = LevelTerrain {
        world_width_m: Some(16.0),
        ..block()
    };
    let loaded = regia_terrain::load_level_terrain(&root.join("assets"), &cache, LEVEL, Some(&right))
        .expect("matching width");
    assert_eq!(loaded.terrain.expect("terrain").map.world_width(), 16.0);
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-8871: a version-3 index that holds a Z spacing other than X or a valid-data mask is
/// refused by the level load (`level_terrain_mapping`), never read with X's spacing on Z or a
/// missing sample's count as a height; the same cooked map without them loads.
#[test]
fn a_v3_index_with_a_z_spacing_or_a_mask_is_refused_by_the_level_load() {
    let root = regia_test_utils::unique_temp_dir("terrain-mapping-v3-grid");
    let n = 17u32;
    write_project(
        &root,
        n,
        n,
        &mapping_json(n, n, "0.0625", 20_000, "1.0", "-8.0", "-8.0"),
    );
    cook(&root, false);
    let cache = cooked_cache_dir(&root, PLATFORM);
    let path = cache.join(cooked_file_name(terrain_index_asset_id(map_id(&root))));
    let original = std::fs::read(&path).expect("index");
    let cooked = index(&root);
    assert_eq!(cooked.version, TERRAIN_INDEX_FORMAT_VERSION);
    assert_eq!(
        (cooked.source_spacing_z_m, cooked.source_valid_mask.as_ref()),
        (None, None),
        "the cook writes neither entry"
    );
    let load = || regia_terrain::load_level_terrain(&root.join("assets"), &cache, LEVEL, Some(&block()));
    load().expect("the cooked map loads");

    let rewrite = |index: &TerrainIndex| {
        let (mut header, _) = regia_asset::decode_cooked_file(&original).expect("wrapper");
        let payload = index.to_postcard().expect("v3 payload");
        header.payload_len = payload.len() as u64;
        std::fs::write(
            &path,
            regia_asset::encode_cooked_file(&header, &payload).expect("encode"),
        )
        .expect("write index");
    };
    rewrite(&TerrainIndex {
        source_spacing_z_m: Some(0.5),
        ..cooked.clone()
    });
    let err = load().expect_err("a Z spacing other than X");
    assert_eq!(err.code(), "level_terrain_mapping");
    assert!(
        err.to_string().contains("terrain_mapping_unrepresentable"),
        "{err}"
    );

    let mut flags = vec![true; (n * n) as usize];
    flags[40] = false;
    let mask = regia_terrain::source::ValidMask::from_flags(n, n, &flags).expect("mask");
    rewrite(&TerrainIndex {
        source_valid_mask: regia_terrain::TerrainValidMaskIndex::from_valid_mask(&mask),
        ..cooked.clone()
    });
    let err = load().expect_err("a sample without data");
    assert_eq!(err.code(), "level_terrain_mapping");
    assert!(
        err.to_string().contains("terrain_raster_missing_data: 1 of 289"),
        "{err}"
    );

    std::fs::write(&path, &original).expect("restore");
    load().expect("the original loads again");
    let _ = std::fs::remove_dir_all(&root);
}
