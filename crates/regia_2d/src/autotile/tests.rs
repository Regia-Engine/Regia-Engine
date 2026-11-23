// SPDX-License-Identifier: MPL-2.0
use std::collections::BTreeMap;

use super::*;
use crate::tilemap::{Orientation, StaggerAxis, StaggerIndex, TileLayer, TilesetDef};

const IDENTITY16: [u32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

fn identity(n: u32, offset: u32) -> Vec<u32> {
    (offset..offset + n).collect()
}

/// A `width`×`height` map with an empty "ground" layer, tileset "terrain" (GIDs 1..=256) and a
/// second tileset "props" (GIDs 300..=303).
fn map(width: u32, height: u32) -> TileMapAsset {
    let set = |name: &str, first_gid: u32, tile_count: u32| TilesetDef {
        name: name.into(),
        first_gid,
        image: format!("{name}.png"),
        image_width: 128,
        image_height: 256,
        tile_width: 8,
        tile_height: 8,
        columns: 16,
        tile_count,
        spacing: 0,
        margin: 0,
        tiles: Vec::new(),
    };
    TileMapAsset {
        version: 1,
        orientation: Orientation::Orthogonal,
        width,
        height,
        tile_width: 8,
        tile_height: 8,
        background: None,
        tilesets: vec![set("terrain", 1, 256), set("props", 300, 4)],
        layers: vec![MapLayer::Tiles(TileLayer {
            name: "ground".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; (width * height) as usize],
            properties: BTreeMap::new(),
        })],
        properties: BTreeMap::new(),
    }
}

fn terrain(name: &str, layout: Layout) -> Terrain {
    Terrain {
        name: name.into(),
        layout,
        fallback: None,
        members: Vec::new(),
        variants: BTreeMap::new(),
    }
}

fn rules(border: Border, terrains: Vec<Terrain>) -> AutoTileSet {
    AutoTileSet {
        version: AUTOTILE_FORMAT_VERSION,
        tileset: "terrain".into(),
        seed: 0,
        border,
        terrains,
    }
}

fn data(map: &TileMapAsset) -> &[u32] {
    match &map.layers[0] {
        MapLayer::Tiles(t) => &t.data,
        MapLayer::Objects(_) => &[],
    }
}

/// The layer as local ids of the "terrain" tileset (`-` for empty), one string per row.
fn locals(map: &TileMapAsset) -> Vec<String> {
    data(map)
        .chunks(map.width as usize)
        .map(|row| {
            row.iter()
                .map(|&g| {
                    if g == 0 {
                        "-".to_string()
                    } else {
                        (g - 1).to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn block(x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<(u32, u32)> {
    (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| (x, y))).collect()
}

#[test]
fn blob47_has_47_reduced_masks_in_ascending_order() {
    let masks = blob47_masks();
    assert_eq!(masks.len(), 47);
    assert!(masks.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(&masks[..11], &[0, 1, 4, 5, 7, 16, 17, 20, 21, 23, 28]);
    assert_eq!(masks[46], 255);
    assert_eq!(blob47_index(0), 0);
    assert_eq!(blob47_index(255), 46);
    assert_eq!(blob47_index(28), 10);
    // A lone corner (NE without N and E) drops out.
    assert_eq!(reduce_blob_mask(2), 0);
    assert_eq!(blob47_index(1 | 2), blob47_index(1));
    assert_eq!(reduce_blob_mask(1 | 2 | 4), 1 | 2 | 4);
    let distinct: BTreeSet<usize> = (0..=255u8).map(blob47_index).collect();
    assert_eq!(distinct.len(), 47);
}

#[test]
fn edge16_block_single_cell_and_border_modes() {
    let set = rules(
        Border::Empty,
        vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    );
    let mut m = map(5, 5);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &block(1, 1, 3, 3))
        .expect("paints");
    // Masks: N = 1, E = 2, S = 4, W = 8.
    assert_eq!(
        locals(&m),
        [
            "- - - - -",
            "- 6 14 12 -",
            "- 7 15 13 -",
            "- 3 11 9 -",
            "- - - - -"
        ]
    );
    let mut single = map(3, 3);
    bound
        .paint(&mut single, 0, TerrainBrush::Terrain(0), &[(1, 1)])
        .expect("paints");
    assert_eq!(locals(&single), ["- - -", "- 0 -", "- - -"]);

    // Border Empty: a full-width row gets end caps; Border Same: it runs off the map.
    let mut row = map(3, 1);
    bound
        .paint(&mut row, 0, TerrainBrush::Terrain(0), &block(0, 0, 2, 0))
        .expect("paints");
    assert_eq!(locals(&row), ["2 10 8"]);
    let same = rules(
        Border::Same,
        vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    );
    let mut row = map(3, 1);
    same.bind(&row)
        .expect("binds")
        .paint(&mut row, 0, TerrainBrush::Terrain(0), &block(0, 0, 2, 0))
        .expect("paints");
    assert_eq!(locals(&row), ["15 15 15"]);
}

#[test]
fn blob47_block_and_ring_pick_the_pinned_tiles() {
    let set = rules(
        Border::Empty,
        vec![terrain("ground", Layout::Blob47(identity(47, 0)))],
    );
    let mut m = map(5, 5);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &block(1, 1, 3, 3))
        .expect("paints");
    // Top-left corner: E | SE | S = 28 → index 10; centre: 255 → 46; top edge: E SE S SW W = 124.
    assert_eq!(blob47_index(4 | 8 | 16 | 32 | 64), 31);
    assert_eq!(
        locals(&m),
        [
            "- - - - -",
            "- 10 31 26 -",
            "- 12 46 42 -",
            "- 4 36 34 -",
            "- - - - -"
        ]
    );
    // Erasing the centre gives a ring: the same layer as painting the ring directly.
    bound
        .paint(&mut m, 0, TerrainBrush::Erase, &[(2, 2)])
        .expect("erases");
    let mut ring = map(5, 5);
    let cells: Vec<_> = block(1, 1, 3, 3).into_iter().filter(|&c| c != (2, 2)).collect();
    bound
        .paint(&mut ring, 0, TerrainBrush::Terrain(0), &cells)
        .expect("paints");
    assert_eq!(data(&m), data(&ring));
    // Ring corner: E | S (the SE cell is the hole) = 20 → index 7; the top edge loses its
    // south corners with its S edge: E | W = 68 → 15.
    assert_eq!(locals(&ring)[1], "- 7 15 18 -");
}

#[test]
fn pattern_rules_first_match_fallback_and_flips() {
    let rule = |pattern: &str, tile: u32| PatternRule {
        pattern: pattern.into(),
        tile,
        flip_x: false,
        flip_y: false,
        diagonal: false,
    };
    let mut path = terrain(
        "path",
        Layout::Rules(vec![
            // Vertical run: a neighbour above and below.
            rule("?#? / ?#? / ?#?", 20),
            // Left end of a horizontal run, mirrored for the right end.
            rule("??? / .## / ???", 21),
            PatternRule {
                flip_x: true,
                ..rule("??? / ##. / ???", 21)
            },
            rule("??? / ### / ???", 22),
        ]),
    );
    path.fallback = Some(23);
    let set = rules(Border::Empty, vec![path]);
    let mut m = map(4, 3);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &block(0, 1, 3, 1))
        .expect("paints");
    let row: Vec<u32> = data(&m)[4..8].to_vec();
    assert_eq!(row, vec![22, 23, 23, 22 | FLIP_X_BIT]);
    // A lone cell matches nothing: the fallback.
    let mut lone = map(3, 3);
    bound
        .paint(&mut lone, 0, TerrainBrush::Terrain(0), &[(1, 1)])
        .expect("paints");
    assert_eq!(data(&lone)[4], 24);
    // A column matches the first rule even though the row rules would match too at a crossing.
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(1, 0), (1, 2)])
        .expect("paints");
    assert_eq!(data(&m)[5], 21);
    assert!(matches!(
        compile_pattern("###/#.#/###"),
        Err(e) if e.contains("centre")
    ));
    assert!(compile_pattern("##").is_err());
    assert!(compile_pattern("#########?").is_err());
    assert!(compile_pattern("###x#####").is_err());
}

/// A deterministic cell list (64-bit LCG, Knuth's MMIX constants).
fn scattered(seed: u64, count: usize, width: u32, height: u32) -> Vec<(u32, u32)> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let v = state >> 33;
            (
                (v % u64::from(width)) as u32,
                ((v / 7) % u64::from(height)) as u32,
            )
        })
        .collect()
}

fn two_terrains() -> AutoTileSet {
    let mut grass = terrain("grass", Layout::Blob47(identity(47, 0)));
    grass.variants.insert(
        46,
        vec![
            WeightedTile {
                tile: 46,
                weight: 3,
                flip_x: false,
                flip_y: false,
                diagonal: false,
            },
            WeightedTile {
                tile: 47,
                weight: 1,
                flip_x: false,
                flip_y: false,
                diagonal: false,
            },
        ],
    );
    let water = terrain("water", Layout::Edge16(identity(16, 64)));
    AutoTileSet {
        seed: 7,
        ..rules(Border::Same, vec![grass, water])
    }
}

#[test]
fn stroke_order_does_not_change_the_result() {
    let set = two_terrains();
    let strokes: Vec<(TerrainBrush, Vec<(u32, u32)>)> = (0..12u64)
        .map(|i| {
            let brush = match i % 3 {
                0 => TerrainBrush::Terrain(0),
                1 => TerrainBrush::Terrain(1),
                _ => TerrainBrush::Erase,
            };
            (brush, scattered(i + 1, 40, 24, 16))
        })
        .collect();
    let mut forward = map(24, 16);
    let bound = set.bind(&forward).expect("binds");
    for (brush, cells) in &strokes {
        bound.paint(&mut forward, 0, *brush, cells).expect("paints");
    }
    // The same strokes, each one's cells in reverse and split one cell at a time.
    let mut single = map(24, 16);
    for (brush, cells) in &strokes {
        for cell in cells.iter().rev() {
            bound.paint(&mut single, 0, *brush, &[*cell]).expect("paints");
        }
    }
    assert_eq!(data(&forward), data(&single));
    // Re-resolving the whole layer changes nothing.
    assert!(bound.apply_layer(&mut forward, 0).expect("applies").is_empty());
    // Disjoint strokes commute.
    let a = block(0, 0, 5, 5);
    let b = block(10, 8, 20, 14);
    let mut ab = map(24, 16);
    let mut ba = map(24, 16);
    bound.paint(&mut ab, 0, TerrainBrush::Terrain(0), &a).expect("a");
    bound.paint(&mut ab, 0, TerrainBrush::Terrain(1), &b).expect("b");
    bound.paint(&mut ba, 0, TerrainBrush::Terrain(1), &b).expect("b");
    bound.paint(&mut ba, 0, TerrainBrush::Terrain(0), &a).expect("a");
    assert_eq!(data(&ab), data(&ba));
}

#[test]
fn changes_undo_and_redo_exactly() {
    let set = two_terrains();
    let mut m = map(16, 16);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &block(2, 2, 9, 9))
        .expect("paints");
    let before = data(&m).to_vec();
    let changes = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(1), &block(5, 5, 12, 12))
        .expect("paints");
    assert!(changes.windows(2).all(|w| (w[0].y, w[0].x) < (w[1].y, w[1].x)));
    assert!(changes.iter().all(|c| c.before != c.after));
    let after = data(&m).to_vec();
    revert_changes(&mut m, 0, &changes).expect("reverts");
    assert_eq!(data(&m), before.as_slice());
    apply_changes(&mut m, 0, &changes).expect("redoes");
    assert_eq!(data(&m), after.as_slice());
    // Only the painted cells and their neighbours were touched.
    assert!(
        changes
            .iter()
            .all(|c| (4..=13).contains(&c.x) && (4..=13).contains(&c.y))
    );
    assert_eq!(
        revert_changes(
            &mut m,
            0,
            &[TileChange {
                x: 99,
                y: 0,
                before: 0,
                after: 0
            }]
        ),
        Err(AutoTileError::OutsideMap { x: 99, y: 0 })
    );
}

#[test]
fn neighbouring_terrains_count_as_other_and_foreign_tiles_are_overwritten() {
    let set = two_terrains();
    let mut m = map(4, 1);
    let bound = set.bind(&m).expect("binds");
    // A prop tile (other tileset) sits in cell 3; terrain "grass" ids are members, "water" is
    // offset by 64.
    assert!(m.set_tile(0, 3, 0, 300));
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(1), &[(0, 0), (1, 0)])
        .expect("water");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(2, 0)])
        .expect("grass");
    // Water: border Same gives N and S; cell 0 has W (off map) and E; cell 1 has W only.
    assert_eq!(data(&m)[0], 64 + 15 + 1);
    assert_eq!(data(&m)[1], 64 + (1 | 4 | 8) + 1);
    // Grass: N, S and the off-map corners are "same" but E (a prop) and W (water) are not.
    assert_eq!(data(&m)[2], blob47_index(1 | 16) as u32 + 1);
    assert_eq!(data(&m)[3], 300);
    assert_eq!(bound.terrain_of(data(&m)[0]), Some(1));
    assert_eq!(bound.terrain_of(300), None);
    assert_eq!(bound.terrain_of(0), None);
    assert_eq!(bound.terrain_at(&m, 0, 2, 0), Some(0));
    assert_eq!(bound.terrain_at(&m, 0, 9, 0), None);
    assert_eq!(bound.resolve(&m, 0, 3, 0), None);
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(3, 0)])
        .expect("over the prop");
    assert_eq!(bound.terrain_at(&m, 0, 3, 0), Some(0));
}

#[test]
fn variants_follow_the_cell_and_the_weights() {
    let set = two_terrains();
    let mut m = map(128, 128);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &block(0, 0, 127, 127))
        .expect("paints");
    let (mut base, mut alt) = (0u32, 0u32);
    for &g in data(&m) {
        match g - 1 {
            46 => base += 1,
            47 => alt += 1,
            other => panic!("unexpected tile {other}"),
        }
    }
    // Weights 3 : 1 over 16 384 cells: 25 % ± 1.5 %.
    let share = f64::from(alt) / f64::from(base + alt);
    assert!((0.235..0.265).contains(&share), "variant share {share}");
    // The pick is a function of (seed, x, y): repainting a cell keeps it.
    let cell = data(&m)[(5 * 128 + 9) as usize];
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(9, 5)])
        .expect("paints");
    assert_eq!(data(&m)[(5 * 128 + 9) as usize], cell);
    // Another seed gives another layout.
    let reseeded = AutoTileSet {
        seed: 8,
        ..set.clone()
    };
    let mut other = map(128, 128);
    reseeded
        .bind(&other)
        .expect("binds")
        .paint(&mut other, 0, TerrainBrush::Terrain(0), &block(0, 0, 127, 127))
        .expect("paints");
    assert_ne!(data(&m), data(&other));
    assert_eq!(variant_hash(7, 9, 5, 46), variant_hash(7, 9, 5, 46));
    assert_ne!(variant_hash(7, 9, 5, 46), variant_hash(7, 5, 9, 46));
}

#[test]
fn paint_checks_every_cell_before_writing() {
    let set = two_terrains();
    let mut m = map(4, 4);
    let bound = set.bind(&m).expect("binds");
    let err = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0), (4, 0)])
        .expect_err("outside");
    assert_eq!(err, AutoTileError::OutsideMap { x: 4, y: 0 });
    assert_eq!(err.code(), "tile_outside_map");
    assert!(data(&m).iter().all(|&g| g == 0), "nothing written");
    assert_eq!(
        bound.paint(&mut m, 0, TerrainBrush::Terrain(5), &[(0, 0)]),
        Err(AutoTileError::UnknownTerrain(5))
    );
    assert_eq!(
        bound.paint(&mut m, 3, TerrainBrush::Erase, &[(0, 0)]),
        Err(AutoTileError::NotTileLayer(3))
    );
    assert_eq!(AutoTileError::NotTileLayer(3).code(), "tile_not_tile_layer");
    assert_eq!(AutoTileError::UnknownTerrain(5).code(), "tile_unknown_terrain");
    assert!(
        bound
            .paint(&mut m, 0, TerrainBrush::Erase, &[])
            .expect("empty")
            .is_empty()
    );
}

#[test]
fn validation_and_bind_errors() {
    let ok = two_terrains();
    assert!(ok.validate().is_ok());
    let invalid = |set: &AutoTileSet, needle: &str| {
        let err = set.validate().expect_err(needle).to_string();
        assert!(err.contains(needle), "{err} lacks {needle}");
    };
    let mut short = ok.clone();
    short.terrains[1].layout = Layout::Edge16(vec![0; 15]);
    invalid(&short, "16 tiles");
    let mut blob = ok.clone();
    blob.terrains[0].layout = Layout::Blob47(vec![0; 16]);
    invalid(&blob, "47 tiles");
    let mut dup = ok.clone();
    dup.terrains[1].name = "grass".into();
    invalid(&dup, "defined twice");
    let mut overlap = ok.clone();
    overlap.terrains[1].members.push(3);
    invalid(&overlap, "belongs to both");
    let mut weight = ok.clone();
    if let Some(alts) = weight.terrains[0].variants.get_mut(&46) {
        alts[0].weight = 0;
    }
    invalid(&weight, "weight 0");
    let mut fallback = ok.clone();
    fallback.terrains[0].fallback = Some(1);
    invalid(&fallback, "Rules layouts only");
    let mut pattern = ok.clone();
    pattern.terrains[1].layout = Layout::Rules(vec![PatternRule {
        pattern: "#".into(),
        tile: 64,
        flip_x: false,
        flip_y: false,
        diagonal: false,
    }]);
    invalid(&pattern, "1 cells");
    let mut empty_rules = ok.clone();
    empty_rules.terrains[1].layout = Layout::Rules(Vec::new());
    invalid(&empty_rules, "1..=1024 rules");
    let mut none = ok.clone();
    none.terrains.clear();
    invalid(&none, "terrains");
    let mut no_name = ok.clone();
    no_name.tileset.clear();
    invalid(&no_name, "tileset name");
    assert_eq!(AutoTileError::Invalid(String::new()).code(), "tile_bad_rules");

    let mut wrong_set = ok.clone();
    wrong_set.tileset = "props".into();
    let err = wrong_set.bind(&map(2, 2)).expect_err("ids out of range");
    assert!(
        matches!(&err, AutoTileError::Bind(m) if m.contains("4 tiles")),
        "{err}"
    );
    assert_eq!(err.code(), "tile_bind_failed");
    let mut missing = ok.clone();
    missing.tileset = "nope".into();
    assert!(matches!(missing.bind(&map(2, 2)), Err(AutoTileError::Bind(m)) if m.contains("nope")));
    let mut registry = AutoTiles::default();
    assert!(registry.insert("bad", none).is_err());
    registry.insert("ok", ok).expect("registers");
    assert!(registry.get("ok").is_some() && registry.get("bad").is_none());
}

#[test]
fn ron_round_trip_version_gate_and_fingerprint() {
    let set = two_terrains();
    let text = set.to_ron().expect("serializes");
    assert_eq!(AutoTileSet::from_ron(&text).expect("parses"), set);
    let newer = text.replacen(
        &format!("version: {AUTOTILE_FORMAT_VERSION}"),
        &format!("version: {}", AUTOTILE_FORMAT_VERSION + 1),
        1,
    );
    assert_eq!(
        AutoTileSet::from_ron(&newer),
        Err(AutoTileError::Version {
            found: AUTOTILE_FORMAT_VERSION + 1,
            expected: AUTOTILE_FORMAT_VERSION
        })
    );
    assert!(matches!(
        AutoTileSet::from_ron("(version: 1, tileset: 3)"),
        Err(AutoTileError::Parse(_))
    ));
    let mut old = set.clone();
    old.version = 0;
    assert!(matches!(
        old.migrate_to_latest(),
        Err(AutoTileError::Migration(_))
    ));
    assert_eq!(
        autotile_migration_sources(),
        vec![1],
        "v1 -> v2 (WangCorner) is the only step"
    );
    let fp = autotile_schema_fingerprint();
    for needle in [
        "tileset", "seed", "border", "pattern", "weight", "Blob47", "Edge16", "Rules", "Empty",
    ] {
        assert!(fp.contains(needle), "{needle} missing from {fp}");
    }
    assert_eq!(set.terrains[0].layout.name(), "blob47");
    assert_eq!(set.terrain_index("water"), Some(1));
    assert_eq!(set.terrain_index("lava"), None);
}

#[test]
fn the_published_example_parses_binds_and_paints() {
    let text = include_str!("../../../../docs/formats/autotile.example.ron");
    let set = AutoTileSet::from_ron(text).expect("docs/formats/autotile.example.ron parses");
    let mut m = map(8, 8);
    let bound = set.bind(&m).expect("binds to a 256-tile `terrain` tileset");
    for (index, _) in set.terrains.iter().enumerate() {
        bound
            .paint(
                &mut m,
                0,
                TerrainBrush::Terrain(index),
                &block(0, index as u32 * 2, 3, index as u32 * 2),
            )
            .expect("paints");
    }
    assert!(bound.apply_layer(&mut m, 0).expect("applies").is_empty());
}

#[test]
fn apply_layer_fixes_hand_placed_members() {
    let set = rules(
        Border::Empty,
        vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    );
    let mut m = map(3, 1);
    // Three "isolated" wall tiles written by hand: re-resolving joins them.
    for x in 0..3 {
        assert!(m.set_tile(0, x, 0, 1));
    }
    let bound = set.bind(&m).expect("binds");
    let changes = bound.apply_layer(&mut m, 0).expect("applies");
    assert_eq!(locals(&m), ["2 10 8"]);
    assert_eq!(changes.len(), 3);
    assert_eq!(bound.apply_layer(&mut m, 2), Err(AutoTileError::NotTileLayer(2)));
}

#[test]
fn paint_refuses_a_layer_of_the_wrong_length() {
    // `TileMapAsset::from_ron` would refuse this layer; a map built in code does not pass there.
    let mut m = map(2, 2);
    let set = AutoTileSet {
        version: 1,
        tileset: "terrain".into(),
        seed: 0,
        border: Border::Empty,
        terrains: vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    };
    let bound = set.bind(&m).expect("binds");
    if let Some(MapLayer::Tiles(t)) = m.layers.first_mut() {
        t.data.truncate(1);
    }
    let err = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
        .expect_err("short layer");
    assert!(matches!(err, AutoTileError::Bind(_)), "{err}");
}

#[test]
fn apply_layer_and_undo_refuse_a_layer_of_the_wrong_length() {
    let mut m = map(2, 2);
    let set = AutoTileSet {
        version: 1,
        tileset: "terrain".into(),
        seed: 0,
        border: Border::Empty,
        terrains: vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    };
    let bound = set.bind(&m).expect("binds");
    if let Some(MapLayer::Tiles(t)) = m.layers.first_mut() {
        t.data.clear();
    }
    let err = bound.apply_layer(&mut m, 0).expect_err("empty layer");
    assert!(matches!(err, AutoTileError::Bind(_)), "{err}");
    let change = TileChange {
        x: 0,
        y: 0,
        before: 0,
        after: 1,
    };
    let err = revert_changes(&mut m, 0, &[change]).expect_err("empty layer");
    assert!(matches!(err, AutoTileError::Bind(_)), "{err}");
}

#[test]
fn a_rule_tile_past_the_gid_mask_does_not_bind() {
    let mut m = map(2, 2);
    m.tilesets[0].columns = 0x1000_0002;
    m.tilesets[0].tile_count = 0x1000_0002;
    let mut tiles = IDENTITY16.to_vec();
    tiles[5] = 0x1000_0000;
    let set = AutoTileSet {
        version: 1,
        tileset: "terrain".into(),
        seed: 0,
        border: Border::Empty,
        terrains: vec![terrain("wall", Layout::Edge16(tiles))],
    };
    assert!(matches!(set.bind(&m), Err(AutoTileError::Bind(_))));
}

#[test]
fn bind_rejects_short_layouts_and_empty_variants() {
    let m = map(1, 1);
    let short_edge = rules(Border::Same, vec![terrain("t", Layout::Edge16(vec![0]))]);
    assert!(short_edge.bind(&m).is_err());
    let short_blob = rules(Border::Same, vec![terrain("t", Layout::Blob47(identity(20, 0)))]);
    assert!(short_blob.bind(&m).is_err());
    let mut t = terrain("t", Layout::Edge16(identity(16, 0)));
    t.variants.insert(0, Vec::new());
    assert!(rules(Border::Same, vec![t]).bind(&m).is_err());
}

/// A rule set with one `WangCorner` terrain whose tile `i` is local id `i`.
fn wang_rules() -> AutoTileSet {
    rules(
        Border::Empty,
        vec![terrain("grass", Layout::WangCorner(identity(16, 0)))],
    )
}

/// Paints `cells` and returns the layer as local ids.
fn wang_paint(width: u32, height: u32, cells: &[(u32, u32)]) -> Vec<String> {
    let mut m = map(width, height);
    let set = wang_rules();
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), cells)
        .expect("paints");
    locals(&m)
}

#[test]
fn wang_corner_lone_cell_has_no_inside_corner() {
    assert_eq!(wang_paint(3, 3, &[(1, 1)]), ["- - -", "- 0 -", "- - -"]);
}

#[test]
fn wang_corner_block_picks_corner_masks() {
    // A 3x3 block in a 5x5 map. Corner bits: TL 1, TR 2, BR 4, BL 8. A corner is inside when the
    // cell, both edge neighbours and the diagonal are terrain, so only the block's inner corners
    // count: the centre has all four (15), an edge-middle cell has the two corners facing the
    // centre, a block corner cell has only the one facing the centre.
    let got = wang_paint(5, 5, &block(1, 1, 3, 3));
    assert_eq!(
        got,
        ["- - - - -", "- 4 12 8 -", "- 6 15 9 -", "- 2 3 1 -", "- - - - -",]
    );
}

#[test]
fn wang_corner_is_order_independent_and_matches_apply_layer() {
    let cells: Vec<(u32, u32)> = vec![(1, 1), (2, 1), (3, 1), (2, 2), (2, 3), (4, 3), (0, 4)];
    let mut forward = map(6, 6);
    let mut backward = map(6, 6);
    let set = wang_rules();
    let bf = set.bind(&forward).expect("binds");
    for &c in &cells {
        bf.paint(&mut forward, 0, TerrainBrush::Terrain(0), &[c])
            .expect("paints");
    }
    let bb = set.bind(&backward).expect("binds");
    for &c in cells.iter().rev() {
        bb.paint(&mut backward, 0, TerrainBrush::Terrain(0), &[c])
            .expect("paints");
    }
    assert_eq!(data(&forward), data(&backward));
    let mut again = forward.clone();
    assert!(bf.apply_layer(&mut again, 0).expect("applies").is_empty());
}

#[test]
fn wang_corner_needs_16_tiles_and_round_trips() {
    let bad = rules(
        Border::Same,
        vec![terrain("g", Layout::WangCorner(identity(15, 0)))],
    );
    assert!(matches!(bad.validate(), Err(AutoTileError::Invalid(m)) if m.contains("WangCorner needs 16")));
    let set = wang_rules();
    assert_eq!(
        AutoTileSet::from_ron(&set.to_ron().expect("ron")).expect("parses"),
        set
    );
}

#[test]
fn v1_files_load_through_the_migration() {
    let text = "(version: 1, tileset: \"terrain\", terrains: [(name: \"w\", layout: Edge16([0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]))])";
    let set = AutoTileSet::from_ron(text).expect("v1 reads");
    assert_eq!(set.version, AUTOTILE_FORMAT_VERSION);
    assert_eq!(autotile_migration_sources(), vec![1]);
}

/// A map with the given orientation.
fn oriented(width: u32, height: u32, orientation: Orientation) -> TileMapAsset {
    let mut m = map(width, height);
    m.orientation = orientation;
    m
}

/// Rules that read two hexagon-only facts: a neighbor to the north-east (tile 5), to the
/// south-west (tile 6), both (7), neither (0).
fn diagonal_rules() -> AutoTileSet {
    let rule = |pattern: &str, tile: u32| PatternRule {
        pattern: pattern.into(),
        tile,
        flip_x: false,
        flip_y: false,
        diagonal: false,
    };
    let mut t = terrain(
        "t",
        Layout::Rules(vec![
            rule("??# / ?#? / #??", 7),
            rule("??# / ?#? / ???", 5),
            rule("??? / ?#? / #??", 6),
        ]),
    );
    t.fallback = Some(0);
    rules(Border::Empty, vec![t])
}

#[test]
fn hex_map_autotile_reads_the_geometric_neighbors() {
    let hex = Orientation::Hexagonal {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Odd,
        side_length: 4,
    };
    let paint = |orientation| {
        let mut m = oriented(5, 5, orientation);
        let set = diagonal_rules();
        let bound = set.bind(&m).expect("binds");
        // A diagonal chain going down-left on a pointy odd-shifted hex grid:
        // (2,1) odd row -> SW (2,2) even row -> SW (1,3) odd row.
        bound
            .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(2, 1), (2, 2), (1, 3)])
            .expect("paints");
        let g = |x: usize, y: usize| data(&m)[y * 5 + x];
        (g(2, 1), g(2, 2), g(1, 3))
    };
    // Hex: the chain's ends have one diagonal neighbor each and the middle has both.
    // Local ids are offset by first_gid 1.
    assert_eq!(paint(hex), (6 + 1, 7 + 1, 5 + 1));
    // The same cells on a square grid are not diagonal neighbors of one another in that sense.
    assert_ne!(paint(Orientation::Orthogonal), paint(hex));
    // The 8-neighbor fallback of the old matcher gave a different answer on every hex map.
    let flat = Orientation::Hexagonal {
        axis: StaggerAxis::X,
        index: StaggerIndex::Odd,
        side_length: 4,
    };
    let mut m = oriented(5, 5, flat);
    let set = diagonal_rules();
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(1, 1), (2, 1)])
        .expect("paints");
    // Flat odd: column 1 is shifted down, so (1,1)'s NE is (2,1); (2,1)'s SW is (1,1).
    assert_eq!((data(&m)[6], data(&m)[7]), (5 + 1, 6 + 1));
}

#[test]
fn hex_autotile_paint_and_apply_layer_agree_and_undo() {
    let hex = Orientation::Hexagonal {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Even,
        side_length: 4,
    };
    let mut m = oriented(6, 6, hex);
    let set = diagonal_rules();
    let bound = set.bind(&m).expect("binds");
    let original = m.clone();
    let changes = bound
        .paint(
            &mut m,
            0,
            TerrainBrush::Terrain(0),
            &[(1, 1), (2, 2), (2, 3), (3, 4)],
        )
        .expect("paints");
    let mut again = m.clone();
    assert!(bound.apply_layer(&mut again, 0).expect("applies").is_empty());
    revert_changes(&mut m, 0, &changes).expect("reverts");
    assert_eq!(data(&m), data(&original));
}

#[test]
fn edge16_mask_is_n_e_s_w_bits() {
    assert_eq!(edge16_mask(false, false, false, false), 0);
    assert_eq!(edge16_mask(true, false, false, false), 1);
    assert_eq!(edge16_mask(false, true, false, false), 2);
    assert_eq!(edge16_mask(false, false, true, false), 4);
    assert_eq!(edge16_mask(false, false, false, true), 8);
    assert_eq!(edge16_mask(true, true, true, true), 15);
}

#[test]
fn pattern_length_counts_cells_not_spacing() {
    let padded = format!("{}?#? / ### / ?.?", " ".repeat(70));
    assert!(compile_pattern(&padded).is_ok());
    assert!(compile_pattern(&format!("{}#########?", " ".repeat(70))).is_err());
}

#[test]
fn short_layer_reports_bind_failed() {
    let set = two_terrains();
    let mut m = map(2, 2);
    if let Some(MapLayer::Tiles(t)) = m.layers.first_mut() {
        t.data.pop();
    }
    let bound = set.bind(&m).expect("binds");
    let err = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
        .expect_err("short layer");
    assert!(
        matches!(&err, AutoTileError::Bind(msg) if msg.contains("2x2")),
        "{err}"
    );
    assert_eq!(err.code(), "tile_bind_failed");
}

/// Spec 86 § 3: `edge16_mask` is the Edge16 index the painter uses (N = 1, E = 2, S = 4, W = 8).
#[test]
fn edge16_mask_is_the_index_the_painter_uses() {
    assert_eq!(edge16_mask(false, false, false, false), 0);
    assert_eq!(edge16_mask(true, false, false, false), 1);
    assert_eq!(edge16_mask(false, true, false, false), 2);
    assert_eq!(edge16_mask(false, false, true, false), 4);
    assert_eq!(edge16_mask(false, false, false, true), 8);
    assert_eq!(edge16_mask(true, true, true, true), 15);
    let all: BTreeSet<usize> = (0..16u8)
        .map(|m| edge16_mask(m & 1 != 0, m & 2 != 0, m & 4 != 0, m & 8 != 0))
        .collect();
    assert_eq!(all, (0..16).collect());
    // A plus shape: the centre sees all four, each arm sees only the centre.
    let set = rules(
        Border::Empty,
        vec![terrain("wall", Layout::Edge16(IDENTITY16.to_vec()))],
    );
    let mut m = map(3, 3);
    set.bind(&m)
        .expect("binds")
        .paint(
            &mut m,
            0,
            TerrainBrush::Terrain(0),
            &[(1, 0), (0, 1), (1, 1), (2, 1), (1, 2)],
        )
        .expect("paints");
    let s = edge16_mask(false, false, true, false);
    let w = edge16_mask(false, false, false, true);
    let e = edge16_mask(false, true, false, false);
    let n = edge16_mask(true, false, false, false);
    assert_eq!(
        locals(&m),
        [
            format!("- {s} -"),
            format!("{e} {} {w}", edge16_mask(true, true, true, true)),
            format!("- {n} -"),
        ]
    );
}

/// Spec 86 § 8 item 2: Blob47 over a single cell and a line (the block and ring are pinned in
/// `blob47_block_and_ring_pick_the_pinned_tiles`).
#[test]
fn blob47_single_cell_and_line_pick_the_pinned_tiles() {
    let set = rules(
        Border::Empty,
        vec![terrain("ground", Layout::Blob47(identity(47, 0)))],
    );
    let mut single = map(3, 3);
    let bound = set.bind(&single).expect("binds");
    bound
        .paint(&mut single, 0, TerrainBrush::Terrain(0), &[(1, 1)])
        .expect("paints");
    assert_eq!(locals(&single), ["- - -", "- 0 -", "- - -"]);
    // A horizontal line: the ends see E (4 → 2) and W (64 → 13), the middle E | W (68 → 15).
    assert_eq!(blob47_index(4), 2);
    assert_eq!(blob47_index(64), 13);
    assert_eq!(blob47_index(4 | 64), 15);
    let mut line = map(5, 3);
    bound
        .paint(&mut line, 0, TerrainBrush::Terrain(0), &block(1, 1, 3, 1))
        .expect("paints");
    assert_eq!(locals(&line), ["- - - - -", "- 2 15 13 -", "- - - - -"]);
    // A vertical line: N = 1 → 1, S = 16 → 5, N | S = 17 → 6.
    assert_eq!(blob47_index(16), 5);
    assert_eq!(blob47_index(1 | 16), 6);
    let mut column = map(3, 5);
    bound
        .paint(&mut column, 0, TerrainBrush::Terrain(0), &block(1, 1, 1, 3))
        .expect("paints");
    assert_eq!(locals(&column), ["- - -", "- 5 -", "- 6 -", "- 1 -", "- - -"]);
}

/// Spec 86 § 4 / § 8 item 7: every validated limit holds at its maximum and refuses one more.
#[test]
fn validation_limits_hold_at_the_maximum_and_refuse_one_more() {
    let rule = |tile: u32| PatternRule {
        pattern: "?#? / ### / ?#?".into(),
        tile,
        flip_x: false,
        flip_y: false,
        diagonal: false,
    };
    let invalid = |set: &AutoTileSet, needle: &str| {
        let err = set.validate().expect_err(needle);
        assert_eq!(err.code(), "tile_bad_rules");
        assert!(err.to_string().contains(needle), "{err} lacks {needle}");
    };
    // Terrains: 256 (disjoint one-rule terrains) pass, 257 do not.
    let many = |n: u32| {
        rules(
            Border::Same,
            (0..n)
                .map(|i| terrain(&format!("t{i}"), Layout::Rules(vec![rule(i)])))
                .collect(),
        )
    };
    assert!(many(MAX_TERRAINS as u32).validate().is_ok());
    invalid(&many(MAX_TERRAINS as u32 + 1), "1..=256 terrains");
    // Rules per terrain: 1024 pass, 1025 do not.
    let rule_count = |n: usize| rules(Border::Same, vec![terrain("r", Layout::Rules(vec![rule(0); n]))]);
    assert!(rule_count(MAX_RULES).validate().is_ok());
    invalid(&rule_count(MAX_RULES + 1), "1..=1024 rules");
    // Variants per tile: 64 pass, 65 do not; weights 1..=65 535.
    let variants = |n: u32, weight: u32| {
        let mut t = terrain("v", Layout::Edge16(IDENTITY16.to_vec()));
        let alts = (0..n)
            .map(|i| WeightedTile {
                tile: 100 + i,
                weight,
                flip_x: false,
                flip_y: false,
                diagonal: false,
            })
            .collect();
        t.variants.insert(15, alts);
        rules(Border::Same, vec![t])
    };
    assert!(variants(MAX_VARIANTS as u32, MAX_WEIGHT).validate().is_ok());
    invalid(&variants(MAX_VARIANTS as u32 + 1, 1), "1..=64 entries");
    invalid(&variants(1, MAX_WEIGHT + 1), "weight 65536");
    // Names: 256 bytes pass, 257 do not (terrain and tileset).
    let named = |terrain_name: &str, tileset: &str| {
        let mut set = rules(
            Border::Same,
            vec![terrain(terrain_name, Layout::Edge16(IDENTITY16.to_vec()))],
        );
        set.tileset = tileset.into();
        set
    };
    let long = "n".repeat(256);
    let too_long = "n".repeat(257);
    assert!(named(&long, &long).validate().is_ok());
    invalid(
        &named(&too_long, "terrain"),
        "terrain names must be 1..=256 bytes",
    );
    invalid(&named("wall", &too_long), "tileset name must be 1..=256 bytes");
    // Patterns: 64 bytes (spacing included) pass, 65 do not.
    let padded = |len: usize| {
        let mut r = rule(0);
        r.pattern = format!("{:<len$}", "?#?###?#?");
        rules(Border::Same, vec![terrain("p", Layout::Rules(vec![r]))])
    };
    assert!(padded(64).validate().is_ok());
    invalid(&padded(65), "longer than 64 bytes");
}
