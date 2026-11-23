// SPDX-License-Identifier: MPL-2.0
//! Tiled maps in every orientation, Tiled's Wang sets as `.autotile.ron`, and the cooked autotile
//! (`docs/specs/86-2d-toolkit.md` § 12, ADR-2597). Fixtures are hand-written from the public
//! Tiled format documentation.

use std::path::PathBuf;

use regia_2d::autotile::{AutoTileSet, Layout, TerrainBrush};
use regia_2d::tilemap::{MapLayer, Orientation, StaggerAxis, StaggerIndex, TileMapAsset};
use regia_asset_pipeline::tiled_import::import_tiled;
use regia_asset_pipeline::tiled_wang::{import_wang_autotiles, wang_to_autotile_ron};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_test_utils::unique_temp_dir;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiled")
}

fn import(name: &str) -> TileMapAsset {
    let imported = import_tiled(&fixtures().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(imported.warnings.is_empty(), "{name}: {:?}", imported.warnings);
    imported.map
}

fn ground(map: &TileMapAsset) -> Vec<u32> {
    match &map.layers[0] {
        MapLayer::Tiles(t) => t.data.clone(),
        MapLayer::Objects(_) => Vec::new(),
    }
}

#[test]
fn isometric_tmj_imports_to_the_expected_cells() {
    let map = import("iso_diamond.tmj");
    assert_eq!(map.orientation, Orientation::Isometric);
    assert_eq!(
        (map.width, map.height, map.tile_width, map.tile_height),
        (3, 2, 64, 32)
    );
    assert_eq!(ground(&map), vec![1, 2, 3, 0, 1, 2]);
    assert_eq!(map.tile_at(0, 2, 0).map(|t| t.local_id), Some(2));
    assert!(map.tile_at(0, 0, 1).is_none());
    // The diamond of cell (0, 0) is the top of a 160 x 80 map; its center picks it back.
    assert_eq!(map.pixel_size(), [160, 80]);
    let center = map.tile_world_center(0, 0);
    assert_eq!(map.cell_at_world(center), Some((0, 0)));
}

#[test]
fn staggered_and_hexagonal_maps_keep_their_stagger_settings() {
    assert_eq!(
        import("staggered_x.tmj").orientation,
        Orientation::Staggered {
            axis: StaggerAxis::X,
            index: StaggerIndex::Odd
        }
    );
    assert_eq!(
        import("hex_flat.tmj").orientation,
        Orientation::Hexagonal {
            axis: StaggerAxis::X,
            index: StaggerIndex::Even,
            side_length: 16
        }
    );
    let pointy = import("hex_pointy.tmx");
    assert_eq!(
        pointy.orientation,
        Orientation::Hexagonal {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Even,
            side_length: 16
        }
    );
    assert_eq!(ground(&pointy), vec![1, 2, 3, 4, 0, 1, 2, 3, 4, 0, 1, 2]);
    // Every cell's center picks that cell, on every imported orientation.
    for name in [
        "staggered_x.tmj",
        "hex_flat.tmj",
        "hex_pointy.tmx",
        "iso_diamond.tmj",
    ] {
        let map = import(name);
        for y in 0..map.height {
            for x in 0..map.width {
                assert_eq!(
                    map.cell_at_world(map.tile_world_center(x, y)),
                    Some((x, y)),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn imported_orientation_survives_ron_and_validates() {
    for name in [
        "iso_diamond.tmj",
        "staggered_x.tmj",
        "hex_flat.tmj",
        "hex_pointy.tmx",
    ] {
        let map = import(name);
        let text = map.to_ron().expect("serializes");
        assert_eq!(TileMapAsset::from_ron(&text).expect("parses"), map, "{name}");
    }
}

#[test]
fn wang_corner_tsx_becomes_a_wang_corner_terrain() {
    let sets = import_wang_autotiles(&fixtures().join("wang_corner.tsx")).expect("imports");
    assert_eq!(sets.len(), 1);
    let set = &sets[0].set;
    assert_eq!(set.tileset, "wangcorner");
    assert_eq!(set.terrains.len(), 1);
    assert_eq!(set.terrains[0].name, "Grass");
    let Layout::WangCorner(tiles) = &set.terrains[0].layout else {
        panic!("corner type gives WangCorner");
    };
    // Tile id (mask * 7) % 16 shows the corners of `mask`; mask 0 borrows mask 1's tile.
    let expected: Vec<u32> = (0..16u32).map(|m| (m.max(1) * 7) % 16).collect();
    assert_eq!(tiles, &expected);
    assert!(sets[0].warnings.iter().any(|w| w.contains("nearest")));
}

#[test]
fn imported_wang_rules_pick_the_expected_tiles_on_a_fixture_map() {
    let sets = import_wang_autotiles(&fixtures().join("wang_corner.tsx")).expect("imports");
    let set = &sets[0].set;
    // A 5 x 5 map using the imported tileset; paint a 3 x 3 block.
    let tileset = regia_2d::tilemap::TilesetDef {
        name: "wangcorner".into(),
        first_gid: 1,
        image: "wang.png".into(),
        image_width: 64,
        image_height: 64,
        tile_width: 16,
        tile_height: 16,
        columns: 4,
        tile_count: 16,
        spacing: 0,
        margin: 0,
        tiles: Vec::new(),
    };
    let mut map = TileMapAsset {
        version: regia_2d::TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width: 5,
        height: 5,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![tileset],
        layers: vec![MapLayer::Tiles(regia_2d::tilemap::TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; 25],
            properties: Default::default(),
        })],
        properties: Default::default(),
    };
    let cells: Vec<(u32, u32)> = (1..4).flat_map(|y| (1..4).map(move |x| (x, y))).collect();
    set.bind(&map)
        .expect("binds")
        .paint(&mut map, 0, TerrainBrush::Terrain(0), &cells)
        .expect("paints");
    // Masks of a 3 x 3 block (see the autotile tests): 4 12 8 / 6 15 9 / 2 3 1.
    let masks = [4u32, 12, 8, 6, 15, 9, 2, 3, 1];
    for (i, mask) in masks.iter().enumerate() {
        let (x, y) = (1 + i as u32 % 3, 1 + i as u32 / 3);
        let tile = map.tile_at(0, x, y).expect("painted");
        assert_eq!(tile.local_id, (mask * 7) % 16, "cell ({x}, {y})");
    }
}

#[test]
fn edge_tsj_gives_one_edge16_terrain_per_color_and_skips_transition_tiles() {
    let sets = import_wang_autotiles(&fixtures().join("wang_edge.tsj")).expect("imports");
    let set = &sets[0].set;
    let names: Vec<&str> = set.terrains.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Water", "Sand"]);
    let Layout::Edge16(water) = &set.terrains[0].layout else {
        panic!()
    };
    let Layout::Edge16(sand) = &set.terrains[1].layout else {
        panic!()
    };
    assert_eq!(water[5], 4);
    assert_eq!(sand[5], 19);
    assert!(!set.terrains.iter().any(|t| t.member_tiles().contains(&30)));
}

#[test]
fn legacy_terrains_are_read_as_a_corner_set() {
    let sets = import_wang_autotiles(&fixtures().join("wang_legacy.tsj")).expect("imports");
    let Layout::WangCorner(tiles) = &sets[0].set.terrains[0].layout else {
        panic!()
    };
    // Tile `id == mask`; mask 0 borrows mask 1.
    let expected: Vec<u32> = (0..16u32).map(|m| m.max(1)).collect();
    assert_eq!(tiles, &expected);
}

#[test]
fn unsupported_extension_and_garbage_are_errors() {
    assert!(import_wang_autotiles(&fixtures().join("sample.tmx")).is_ok());
    assert!(import_wang_autotiles(&fixtures().join("tileset.png")).is_err());
    let dir = unique_temp_dir("wang-garbage");
    std::fs::write(dir.join("bad.tsj"), "{ nope").expect("write");
    assert!(import_wang_autotiles(&dir.join("bad.tsj")).is_err());
    std::fs::write(dir.join("bad.tsx"), "<tileset").expect("write");
    assert!(import_wang_autotiles(&dir.join("bad.tsx")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn imported_rules_serialize_to_autotile_ron_that_parses_back() {
    let out = wang_to_autotile_ron(&fixtures().join("wang_edge.tsj")).expect("converts");
    assert_eq!(out.len(), 1);
    let (tileset, text, _) = &out[0];
    assert_eq!(tileset, "edges");
    let set = AutoTileSet::from_ron(text).expect("parses");
    assert_eq!(set.terrains.len(), 2);
}

#[test]
fn cooked_autotile_loads_identical_to_the_raw_file() {
    let project = unique_temp_dir("cook-autotile");
    let assets = project.join("assets/maps");
    std::fs::create_dir_all(&assets).expect("assets");
    // The wang import's own output is the rule file.
    let (_, text, _) = wang_to_autotile_ron(&fixtures().join("wang_corner.tsx"))
        .expect("converts")
        .remove(0);
    std::fs::write(assets.join("ground.autotile.ron"), &text).expect("write");

    let report = cook_project(CookOptions::for_host(project.clone())).expect("cook runs");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = regia_asset::cooked_cache_dir(&project, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("manifest exists");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == "assets/maps/ground.autotile.ron")
        .expect("the rules are cooked");
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(entry.id))).expect("cooked file");

    let mut tiles = regia_2d::AutoTiles::default();
    tiles
        .insert_cooked("ground", &bytes)
        .expect("loads from the cooked file");
    let raw = AutoTileSet::from_ron(&text).expect("raw parses");
    assert_eq!(**tiles.get("ground").expect("registered"), raw);

    // A second cook changes nothing; a damaged or wrong-typed file is an error, not a panic.
    let again = cook_project(CookOptions::for_host(project.clone())).expect("recook");
    assert_eq!(again.cooked, 0, "{again:?}");
    assert!(
        regia_2d::AutoTiles::default()
            .insert_cooked("x", &bytes[..bytes.len() / 2])
            .is_err()
    );
    assert!(
        regia_2d::AutoTiles::default()
            .insert_cooked("x", b"junk")
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_broken_autotile_ron_fails_the_cook() {
    let project = unique_temp_dir("cook-autotile-bad");
    let assets = project.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    std::fs::write(
        assets.join("bad.autotile.ron"),
        "(version: 2, tileset: \"\", terrains: [])",
    )
    .expect("write");
    let report = cook_project(CookOptions::for_host(project.clone())).expect("cook runs");
    assert!(!report.errors.is_empty(), "an empty rule set is rejected");
    let _ = std::fs::remove_dir_all(&project);
}
