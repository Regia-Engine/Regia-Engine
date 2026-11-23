// SPDX-License-Identifier: MPL-2.0
//! ADR-8542 (CT-21): `regia cook` honors an engine raster descriptor (`heightfield.raster.json`)
//! beside `heightfield.png`: a north-first raster is permuted exactly into canonical order before
//! tiling, cell-edge registration resolves into the persisted mapping, and what the terrain index
//! (v2) cannot hold exactly (unequal X/Z spacing, samples without data) is refused, never resampled
//! or filled. CPU only; original fixtures.

use std::path::Path;

use regia_asset::{cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::terrain_cook::terrain_settings_hash;
use regia_asset_pipeline::{CookOptions, CookReport, cook_project};
use regia_terrain::source::{ImportLimits, RasterDescriptor, TerrainSource};
use regia_terrain::stream::{
    decode_cooked_tiled_heightfield, heightfield_asset_id, load_terrain_index_bytes,
    stitch_tiled_heightfield, terrain_index_asset_id,
};

const DIR: &str = "assets/maps/basin";
const LAYER: &str = "assets/maps/basin/basin.terrain.layers.ron";
const PLATFORM: &str = "linux";
const N: u32 = 9;

/// Canonical (south-first) counts with a nonzero datum.
fn canonical() -> Vec<u16> {
    (0..N)
        .flat_map(|r| (0..N).map(move |c| 19_900 + ((c * 29 + r * 53) % 211) as u16))
        .collect()
}

/// The same grid stored north-first (rows reversed).
fn north_first(counts: &[u16]) -> Vec<u16> {
    counts.chunks(N as usize).rev().flatten().copied().collect()
}

const UNITS: &str = r#"{"schema": 2, "grid": {"cols": 9, "rows": 9, "cell_m": 1.0},
  "units": {"kind": "counts", "meters_per_count": 0.03125, "zero_counts": 20000}}"#;

fn write_project(root: &Path, counts: Vec<u16>, json: Option<&str>, raster: Option<&str>) {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(
        root.join(LAYER),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .expect("layers");
    let image = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(N, N, counts).expect("image");
    image.save(dir.join("heightfield.png")).expect("png");
    if let Some(json) = json {
        std::fs::write(dir.join("heightfield.json"), json).expect("heightfield.json");
    }
    if let Some(raster) = raster {
        std::fs::write(dir.join("heightfield.raster.json"), raster).expect("raster");
    }
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: PLATFORM.into(),
        incremental: false,
    })
    .expect("cook runs")
}

fn map_id(root: &Path) -> regia_asset::AssetId {
    let meta = std::fs::read(regia_asset::meta_path_for(&root.join(LAYER))).expect("layer meta");
    regia_asset::MetaSidecar::from_ron(&meta).expect("meta").id
}

/// The cooked counts (stitched tiles) and the index's mapping.
fn cooked(root: &Path) -> (Vec<u16>, regia_terrain::source::CountMapping) {
    let cache = cooked_cache_dir(root, PLATFORM);
    let id = map_id(root);
    let index = std::fs::read(cache.join(cooked_file_name(terrain_index_asset_id(id)))).expect("index");
    let mapping = load_terrain_index_bytes(&index)
        .expect("index decodes")
        .source_mapping
        .expect("mapping");
    let tiled = std::fs::read(cache.join(cooked_file_name(heightfield_asset_id(id)))).expect("tiled");
    let hf =
        stitch_tiled_heightfield(&decode_cooked_tiled_heightfield(&tiled).expect("decode")).expect("stitch");
    (hf.samples, mapping)
}

const NORTH_EDGE: &str = r#"{"raster_descriptor": 1, "spacing_m": [0.5, 0.5], "row0": "north",
  "registration": "edge", "origin_m": [-2.25, -2.25]}"#;
const SOUTH_EDGE: &str = r#"{"raster_descriptor": 1, "spacing_m": [0.5, 0.5], "row0": "south",
  "registration": "edge", "origin_m": [-2.25, -2.25]}"#;

#[test]
fn north_first_and_south_first_rasters_cook_to_the_same_canonical_counts_and_mapping() {
    let north = regia_test_utils::unique_temp_dir("terrain-raster-cook-north");
    let south = regia_test_utils::unique_temp_dir("terrain-raster-cook-south");
    write_project(&north, north_first(&canonical()), Some(UNITS), Some(NORTH_EDGE));
    write_project(&south, canonical(), Some(UNITS), Some(SOUTH_EDGE));
    for root in [&north, &south] {
        let report = cook(root);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }
    let (north_counts, north_mapping) = cooked(&north);
    let (south_counts, south_mapping) = cooked(&south);
    assert_eq!(
        north_counts,
        canonical(),
        "north-first counts are permuted, never interpolated"
    );
    assert_eq!(south_counts, canonical());
    assert_eq!(north_mapping.canonical_bytes(), south_mapping.canonical_bytes());
    // Edge registration: the first sample is half a spacing inside the cell edge.
    assert_eq!(north_mapping.sample0_m, [-2.0, -2.0]);
    assert_eq!(north_mapping.cell_m, 0.5);
    assert_eq!(north_mapping.zero_counts, 20_000);
    // The cooked source equals the importer's view of the same descriptor, bit for bit.
    let descriptor =
        RasterDescriptor::from_json(NORTH_EDGE.as_bytes(), ImportLimits::default()).expect("descriptor");
    let imported = TerrainSource::from_raster(
        north_first(&canonical()),
        [N, N],
        0.03125,
        20_000,
        &descriptor,
        ImportLimits::default(),
    )
    .expect("import");
    assert_eq!(imported.counts(), &north_counts[..]);
    assert_eq!(imported.count_mapping().expect("mapping"), north_mapping);
    // The descriptor is a cook input: changing it changes the settings hash; removing it restores
    // the hash of the same terrain without one.
    let layer = north.join(LAYER);
    let with = terrain_settings_hash(&layer).expect("hash");
    std::fs::write(
        north.join(DIR).join("heightfield.raster.json"),
        NORTH_EDGE.replace("edge", "center"),
    )
    .expect("edit");
    assert_ne!(terrain_settings_hash(&layer).expect("hash"), with);
    std::fs::remove_file(north.join(DIR).join("heightfield.raster.json")).expect("remove");
    let plain = regia_test_utils::unique_temp_dir("terrain-raster-cook-plain");
    write_project(&plain, north_first(&canonical()), Some(UNITS), None);
    let a = terrain_settings_hash(&layer).expect("hash");
    let b = terrain_settings_hash(&plain.join(LAYER)).expect("hash");
    assert_eq!(a, b, "no descriptor: hashes as before");
    for root in [north, south, plain] {
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

fn refusal(raster: &str, json: Option<&str>, counts: Vec<u16>) -> String {
    let root = regia_test_utils::unique_temp_dir("terrain-raster-cook-refusal");
    write_project(&root, counts, json, Some(raster));
    let report = cook(&root);
    let errors = format!("{:?}", report.errors);
    assert!(!report.errors.is_empty(), "the cook must refuse {raster}");
    let cache = cooked_cache_dir(&root, PLATFORM);
    let index = cache.join(cooked_file_name(terrain_index_asset_id(map_id(&root))));
    assert!(
        !index.is_file(),
        "no terrain index is written for a refused raster"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
    errors
}

#[test]
fn the_cook_refuses_missing_data_unequal_spacing_and_descriptors_without_units() {
    let mut counts = canonical();
    counts[40] = 65_535;
    let errors = refusal(
        r#"{"raster_descriptor": 1, "spacing_m": [1, 1], "registration": "center", "nodata_counts": 65535}"#,
        Some(UNITS),
        counts,
    );
    assert!(errors.contains("terrain_raster_missing_data"), "{errors}");
    let errors = refusal(
        r#"{"raster_descriptor": 1, "spacing_m": [1, 0.5], "registration": "center"}"#,
        Some(UNITS),
        canonical(),
    );
    assert!(errors.contains("terrain_mapping_unrepresentable"), "{errors}");
    let errors = refusal(
        r#"{"raster_descriptor": 1, "spacing_m": [1, 1], "registration": "center", "skew_m": [0, 0.5]}"#,
        Some(UNITS),
        canonical(),
    );
    assert!(errors.contains("terrain_raster_transform"), "{errors}");
    let errors = refusal(
        r#"{"raster_descriptor": 1, "spacing_m": [1, 1], "registration": "center"}"#,
        Some(r#"{"size_m": 64}"#),
        canonical(),
    );
    assert!(errors.contains("schema-2 heightfield.json"), "{errors}");
}
