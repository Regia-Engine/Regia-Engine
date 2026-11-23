// SPDX-License-Identifier: MPL-2.0
//! ADR-5294 (supersedes ADR-4914): an Android cook writes a terrain's tiled heightfield, its
//! terrain index and its texture slots as ASTC 4×4 tiles, so a level that names the terrain loads
//! it from the Android cache exactly as on desktop, and no terrain input is cooked generically.
//! CPU only; no GPU.

use std::path::Path;

use regia_asset::{AssetManifest, cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_scene::level::LevelTerrain;
use regia_terrain::stream::{
    heightfield_asset_id, load_terrain_index_bytes, tail_asset_id, terrain_index_asset_id,
};

const SIDE: u32 = 65;
const LAYER: &str = "assets/maps/ground/terrain.terrain.layers.ron";

fn samples() -> Vec<u16> {
    (0..SIDE * SIDE).map(|i| ((i * 37) % 65_536) as u16).collect()
}

fn write_project(root: &Path) {
    let dir = root.join("assets/maps/ground");
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(
        root.join(LAYER),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .expect("layers");
    std::fs::write(dir.join("heightfield.json"), br#"{"size_m":64.0}"#).expect("extents");
    let image = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(SIDE, SIDE, samples())
        .expect("height image");
    image.save(dir.join("heightfield.png")).expect("height png");
    // One 256 tile: the tiled path needs a power of two the tile divides (a multiple of the
    // ASTC 4×4 block).
    let albedo = image::RgbaImage::from_pixel(256, 256, image::Rgba([90, 120, 60, 255]));
    albedo.save(dir.join("macro_albedo.png")).expect("albedo png");
}

fn cook(root: &Path, incremental: bool) -> regia_asset_pipeline::CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: "android".into(),
        incremental,
    })
    .expect("android cook");
    // Every terrain input is read by the terrain cook, none is offered to the generic pass.
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.contains("ASTC tiled cook is not implemented")),
        "the interim warning is gone: {:?}",
        report.warnings
    );
    report
}

#[test]
fn android_cook_writes_the_named_terrain_heightfield_index_and_astc_tiles() {
    let root = regia_test_utils::unique_temp_dir("android-terrain-cook");
    write_project(&root);
    cook(&root, false);

    let cache = cooked_cache_dir(&root, "android");
    let meta_bytes = std::fs::read(regia_asset::meta_path_for(&root.join(LAYER))).expect("layer meta");
    let map_id = regia_asset::MetaSidecar::from_ron(&meta_bytes).expect("meta").id;

    // The index names the heightfield, the world width and the macro albedo slot.
    let index_bytes =
        std::fs::read(cache.join(cooked_file_name(terrain_index_asset_id(map_id)))).expect("terrain index");
    let index = load_terrain_index_bytes(&index_bytes).expect("index decodes");
    // ADR-8471 moved the index to v2 (an optional count mapping); Android writes the desktop format.
    assert_eq!(
        index.version,
        regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
        "the index format is the desktop one"
    );
    assert_eq!(index.source_mapping, None, "a legacy extents file has no mapping");
    let names: Vec<&str> = index.slots.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["macro_albedo"], "the one texture input is a tiled slot");
    assert_eq!((index.slots[0].width, index.slots[0].height), (256, 256));
    let hf = index.heightfield.as_ref().expect("index names the heightfield");
    assert_eq!((hf.width, hf.height), (SIDE, SIDE));
    assert_eq!(index.world_size_m, Some(64.0));
    assert!(
        cache
            .join(cooked_file_name(heightfield_asset_id(map_id)))
            .is_file()
    );

    // The manifest ships the index, the heightfield and the albedo tail (so a pak carries them),
    // and no generic copy of any terrain input.
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("manifest present");
    assert_eq!(manifest.id_for_path(LAYER), Some(terrain_index_asset_id(map_id)));
    assert_eq!(
        manifest.id_for_path(&format!("{LAYER}#terrain/heightfield")),
        Some(heightfield_asset_id(map_id))
    );
    for input in ["macro_albedo.png", "heightfield.png", "heightfield.json"] {
        assert!(
            manifest
                .id_for_path(&format!("assets/maps/ground/{input}"))
                .is_none(),
            "{input} is a terrain input, not a generic asset"
        );
    }
    let tail_id = tail_asset_id(map_id, "macro_albedo");
    assert_eq!(
        manifest.id_for_path(&format!("{LAYER}#terrain/macro_albedo/tail")),
        Some(tail_id)
    );
    // The tail is ASTC 4×4 sRGB (KTX2 vkFormat 158), not BCn.
    let tail = std::fs::read(cache.join(cooked_file_name(tail_id))).expect("albedo tail");
    let (_header, texture) = regia_asset::load_texture_bytes(&tail).expect("tail is a cooked texture");
    let vk = u32::from_le_bytes(texture.ktx2[12..16].try_into().expect("vkFormat"));
    assert_eq!(vk, 158, "ASTC 4x4 sRGB");

    // The runtime consumer: a level naming the terrain loads the source samples bit for bit.
    let block = LevelTerrain {
        layers: "maps/ground/terrain.terrain.layers.ron".into(),
        height_scale_m: 40.0,
        height_offset_m: -10.0,
        ..LevelTerrain::default()
    };
    let loaded =
        regia_terrain::load_level_terrain(&root.join("assets"), &cache, "maps/tools.level.ron", Some(&block))
            .expect("level terrain loads from the android cache");
    let terrain = loaded.terrain.expect("terrain built");
    assert_eq!(terrain.map.to_heightfield().samples, samples());
    assert_eq!(terrain.map.world_width(), 64.0, "width from the index");
    assert_eq!(loaded.map_id, Some(map_id));

    // An unchanged incremental cook skips the terrain and keeps its files.
    let again = cook(&root, true);
    assert!(again.skipped >= 1, "{again:?}");
    assert!(
        cache
            .join(cooked_file_name(heightfield_asset_id(map_id)))
            .is_file()
    );
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("manifest present");
    assert!(
        manifest
            .id_for_path(&format!("{LAYER}#terrain/heightfield"))
            .is_some()
    );
    let _ = std::fs::remove_dir_all(&root);
}
