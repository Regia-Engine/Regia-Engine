// SPDX-License-Identifier: MPL-2.0
//! Aseprite and LDtk cook end to end (ADR-3358): a project with `.aseprite` and `.ldtk` sources
//! cooks into the existing 2D assets (a cooked `SpriteAtlas` of clip strips; a merged world
//! `CookedTileMap` plus `level/<identifier>` sub-assets), writes the import reports, keeps asset
//! and sub-asset ids stable across a reimport, and refuses hostile input with the documented
//! codes. All fixtures are generated in the test (no third-party art or level files).

use std::path::{Path, PathBuf};

use regia_2d::atlas::SpriteAtlas;
use regia_2d::tilemap::CookedTileMap;
use regia_asset::{AssetManifest, cooked_cache_dir, cooked_file_name, decode_cooked_file};
use regia_asset_pipeline::fuzz_entry;
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

fn write_asset(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let path = assets.join(name);
    std::fs::write(&path, bytes).expect("asset");
    path
}

fn read_manifest(root: &Path) -> AssetManifest {
    let cache = cooked_cache_dir(root, host_platform());
    AssetManifest::read_from_cache(&cache)
        .expect("manifest reads")
        .expect("manifest present")
}

fn cooked_payload(root: &Path, id: regia_asset::AssetId) -> Vec<u8> {
    let cache = cooked_cache_dir(root, host_platform());
    let bytes = std::fs::read(cache.join(cooked_file_name(id))).expect("cooked file");
    decode_cooked_file(&bytes)
        .expect("cooked file decodes")
        .1
        .to_vec()
}

#[test]
fn an_aseprite_cooks_to_an_atlas_and_a_report_and_reimports_stably() {
    let root = regia_test_utils::unique_temp_dir("aseprite-cook");
    let path = write_asset(&root, "hero.aseprite", &fuzz_entry::aseprite_fixture());
    let first = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(first.errors.is_empty(), "{:?}", first.errors);

    let manifest = read_manifest(&root);
    let id = manifest
        .id_for_path("assets/hero.aseprite")
        .expect("the atlas is in the manifest");
    let atlas = SpriteAtlas::from_postcard(&cooked_payload(&root, id)).expect("the atlas decodes");
    let idle = atlas
        .sprites
        .iter()
        .find(|s| s.name == "idle")
        .expect("the idle clip's strip");
    assert_eq!(idle.source_size, [16, 8], "two 8x8 frames side by side");

    // The import report the agent method reads.
    let reports: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cooked_cache_dir(&root, host_platform()).join("import_reports.json"))
            .expect("the report file"),
    )
    .expect("the report parses");
    let entry = &reports["entries"]["assets/hero.aseprite"];
    assert_eq!(entry["importer"], "aseprite");
    assert_eq!(entry["data"]["frame_count"], 2);
    assert_eq!(entry["data"]["tags"][0]["name"], "idle");

    // A reimport with no change keeps the id and skips the write.
    let second = cook_project(CookOptions::for_host(root.to_path_buf())).expect("recook");
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(second.skipped, 1, "{second:?}");
    assert_eq!(read_manifest(&root).id_for_path("assets/hero.aseprite"), Some(id));
    let _ = std::fs::remove_dir_all(&root);
    let _ = path;
}

/// The two-level LDtk project of the acceptance test (both layer kinds, one tileset image).
fn two_level_ldtk() -> String {
    r##"{
        "jsonVersion": "1.5.3",
        "externalLevels": false,
        "bgColor": "#000000",
        "iid": "proj",
        "toc": [],
        "worlds": [],
        "levels": [
            {
                "identifier": "A",
                "iid": "a",
                "uid": 1,
                "worldX": 0,
                "worldY": 0,
                "worldDepth": 0,
                "pxWid": 32,
                "pxHei": 16,
                "externalRelPath": null,
                "__bgColor": null,
                "bgRelPath": null,
                "__neighbours": [],
                "fieldInstances": [],
                "layerInstances": [
                    {
                        "__type": "Tiles",
                        "__identifier": "ground",
                        "__gridSize": 8,
                        "__cWid": 4,
                        "__cHei": 2,
                        "__opacity": 1.0,
                        "visible": true,
                        "__pxTotalOffsetX": 0,
                        "__pxTotalOffsetY": 0,
                        "__tilesetDefUid": 7,
                        "gridTiles": [
                            { "t": 0, "f": 0, "px": [0, 0], "src": [0, 0], "a": 1.0 },
                            { "t": 1, "f": 3, "px": [8, 0], "src": [8, 0], "a": 1.0 }
                        ],
                        "autoLayerTiles": [],
                        "intGridCsv": [],
                        "entityInstances": [],
                        "iid": "la",
                        "layerDefUid": 1,
                        "levelId": 1,
                        "pxOffsetX": 0,
                        "pxOffsetY": 0
                    }
                ]
            },
            {
                "identifier": "B",
                "iid": "b",
                "uid": 2,
                "worldX": 32,
                "worldY": 16,
                "worldDepth": 0,
                "pxWid": 32,
                "pxHei": 16,
                "externalRelPath": null,
                "__bgColor": null,
                "bgRelPath": null,
                "__neighbours": [],
                "fieldInstances": [],
                "layerInstances": [
                    {
                        "__type": "Tiles",
                        "__identifier": "ground",
                        "__gridSize": 8,
                        "__cWid": 4,
                        "__cHei": 2,
                        "__opacity": 1.0,
                        "visible": true,
                        "__pxTotalOffsetX": 0,
                        "__pxTotalOffsetY": 0,
                        "__tilesetDefUid": 7,
                        "gridTiles": [
                            { "t": 0, "f": 2, "px": [0, 0], "src": [0, 0], "a": 1.0 }
                        ],
                        "autoLayerTiles": [],
                        "intGridCsv": [],
                        "entityInstances": [],
                        "iid": "lb",
                        "layerDefUid": 1,
                        "levelId": 2,
                        "pxOffsetX": 0,
                        "pxOffsetY": 0
                    }
                ]
            }
        ],
        "defs": {
            "tilesets": [
                {
                    "uid": 7,
                    "identifier": "terrain",
                    "relPath": "ts.png",
                    "pxWid": 16,
                    "pxHei": 8,
                    "tileGridSize": 8,
                    "__cWid": 2,
                    "__cHei": 1,
                    "spacing": 0,
                    "padding": 0,
                    "customData": [],
                    "enumTags": [],
                    "tags": []
                }
            ],
            "layers": [],
            "entities": [],
            "enums": [],
            "externalEnums": [],
            "levelFields": []
        }
    }"##
    .to_string()
}

#[test]
fn an_ldtk_world_cooks_to_a_merged_map_and_level_sub_assets_stably() {
    let root = regia_test_utils::unique_temp_dir("ldtk-cook");
    write_asset(&root, "world.ldtk", two_level_ldtk().as_bytes());
    let image = image::RgbaImage::from_pixel(16, 8, image::Rgba([40, 80, 120, 255]));
    image.save(root.join("assets/ts.png")).expect("tileset image");

    let first = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(first.errors.is_empty(), "{:?}", first.errors);

    let manifest = read_manifest(&root);
    let primary = manifest
        .id_for_path("assets/world.ldtk")
        .expect("the merged map is in the manifest");
    let level_a = manifest
        .id_for_path("assets/world.ldtk#level/A")
        .expect("level A is a sub-asset");
    let level_b = manifest
        .id_for_path("assets/world.ldtk#level/B")
        .expect("level B is a sub-asset");

    // The primary payload is the merged world: B's cells sit at its world position and the layer
    // offset stays zero (the tile batch adds the offset on top of the cell; ldtk_import's module
    // docs, fixed in ddb2c3b3e, whose test update this assertion missed).
    let merged =
        CookedTileMap::from_postcard(&cooked_payload(&root, primary)).expect("the merged map decodes");
    assert_eq!((merged.map.width, merged.map.height), (8, 4));
    assert_eq!(merged.images.len(), 1, "one tileset image embedded");
    let layer_b = merged
        .map
        .layers
        .iter()
        .find_map(|l| match l {
            regia_2d::tilemap::MapLayer::Tiles(t) if t.name == "B/ground" => Some(t),
            _ => None,
        })
        .expect("level B's layer, prefixed");
    assert_eq!(
        layer_b.offset,
        [0.0, 0.0],
        "the world position is in the cells, not doubled"
    );
    assert_eq!(layer_b.data[2 * 8 + 4], 1 | regia_2d::tilemap::FLIP_Y_BIT);
    // One world position: cell (4, 2) of the 8x4 merged map, plus the offset, is B's (32, 16) from
    // the top-left, which is (32, 8) from the map's bottom-left.
    let ([left, bottom], _) = merged.map.tile_world_rect(4, 2);
    let drawn = [
        left + f64::from(layer_b.offset[0]),
        bottom - f64::from(layer_b.offset[1]),
    ];
    assert_eq!(drawn, [32.0, 8.0], "level B is drawn once at its world position");

    // The sub-asset is the level-local map.
    let a = CookedTileMap::from_postcard(&cooked_payload(&root, level_a)).expect("level A decodes");
    assert_eq!((a.map.width, a.map.height), (4, 2));
    let layer_a = a.map.layers.iter().find_map(|l| match l {
        regia_2d::tilemap::MapLayer::Tiles(t) => Some(t),
        _ => None,
    });
    assert!(layer_a.is_some());

    // A reimport with no change keeps every id.
    let second = cook_project(CookOptions::for_host(root.to_path_buf())).expect("recook");
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    let manifest = read_manifest(&root);
    assert_eq!(manifest.id_for_path("assets/world.ldtk"), Some(primary));
    assert_eq!(manifest.id_for_path("assets/world.ldtk#level/A"), Some(level_a));
    assert_eq!(manifest.id_for_path("assets/world.ldtk#level/B"), Some(level_b));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hostile_sources_fail_the_cook_with_their_codes() {
    let root = regia_test_utils::unique_temp_dir("import-hostile");
    let mut truncated = fuzz_entry::aseprite_fixture();
    truncated.truncate(truncated.len() / 2);
    write_asset(&root, "broken.aseprite", &truncated);
    write_asset(&root, "broken.ldtk", b"{ not json");
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    let errors = format!("{:?}", report.errors);
    assert!(errors.contains("aseprite_truncated"), "{errors}");
    assert!(errors.contains("ldtk_json"), "{errors}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn import_reports_tie_the_pipeline_and_agent_file_names() {
    // Written down in two crates; this test ties them (the font report pattern).
    assert_eq!(
        regia_asset_pipeline::import_report::IMPORT_REPORTS_FILE,
        "import_reports.json"
    );
}

/// Corr-low (pipeline-a #24): the LDtk import report was saved before the level maps cooked, so
/// a cook that then failed (here: the tileset image is missing) left a report describing a world
/// that has no cooked map. The report is written only once the cook succeeds.
#[test]
fn corr_low_a_failed_ldtk_cook_writes_no_import_report() {
    let root = regia_test_utils::unique_temp_dir("ldtk-cook-fails");
    write_asset(&root, "world.ldtk", two_level_ldtk().as_bytes());
    // No assets/ts.png: the level maps cannot embed their tileset.
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook runs");
    assert!(
        !report.errors.is_empty(),
        "the missing tileset image fails the cook"
    );
    let reports_path = cooked_cache_dir(&root, host_platform()).join("import_reports.json");
    if let Ok(bytes) = std::fs::read(&reports_path) {
        let reports: serde_json::Value = serde_json::from_slice(&bytes).expect("the report parses");
        assert!(
            reports["entries"]["assets/world.ldtk"].is_null(),
            "a failed cook left an import report: {reports}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
