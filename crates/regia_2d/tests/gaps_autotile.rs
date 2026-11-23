// SPDX-License-Identifier: MPL-2.0
//! Auto-tiling edge cases (test-gap audit): tiny maps and border modes, corner reduction on the
//! map edge, rule priority and fallback, flag handling, painted change lists, hand-placed members,
//! variants versus flips, validation boundaries and error codes. CPU only.
//!
//! Rule sets and maps are written as RON so the tests do not depend on the exact struct fields.

use regia_2d::autotile::{
    AUTOTILE_FORMAT_VERSION, AutoTileError, AutoTileSet, TerrainBrush, TileChange, blob47_index,
};
use regia_2d::tilemap::{FLIP_DIAGONAL_BIT, FLIP_X_BIT, MapLayer, TILEMAP_FORMAT_VERSION, TileMapAsset};

fn ids(first: u32, count: u32) -> String {
    let list: Vec<String> = (first..first + count).map(|i| i.to_string()).collect();
    format!("[{}]", list.join(", "))
}

/// A `w` x `h` map with an empty `ground` layer and an (empty) object layer at index 1. Tileset
/// `terrain` holds gids 1..=256, `props` gids 300..=303.
fn map(w: u32, h: u32) -> TileMapAsset {
    let zeros = vec!["0"; (w * h) as usize].join(", ");
    TileMapAsset::from_ron(&format!(
        r#"(version: {TILEMAP_FORMAT_VERSION}, width: {w}, height: {h}, tile_width: 8, tile_height: 8,
        tilesets: [
          (name: "terrain", first_gid: 1, image: "t.png", image_width: 128, image_height: 128,
           tile_width: 8, tile_height: 8, columns: 16, tile_count: 256),
          (name: "props", first_gid: 300, image: "p.png", image_width: 32, image_height: 8,
           tile_width: 8, tile_height: 8, columns: 4, tile_count: 4)],
        layers: [Tiles((name: "ground", data: [{zeros}])), Objects((name: "o", objects: []))])"#
    ))
    .expect("test map is valid")
}

fn rules(border: &str, terrains: &str) -> AutoTileSet {
    AutoTileSet::from_ron(&format!(
        r#"(version: {AUTOTILE_FORMAT_VERSION}, tileset: "terrain", border: {border}, terrains: [{terrains}])"#
    ))
    .expect("test rules are valid")
}

fn edge16(border: &str) -> AutoTileSet {
    rules(
        border,
        &format!(r#"(name: "wall", layout: Edge16({}))"#, ids(0, 16)),
    )
}

fn blob47(border: &str) -> AutoTileSet {
    rules(
        border,
        &format!(r#"(name: "ground", layout: Blob47({}))"#, ids(0, 47)),
    )
}

fn cells(m: &TileMapAsset) -> Vec<u32> {
    match &m.layers[0] {
        MapLayer::Tiles(t) => t.data.clone(),
        MapLayer::Objects(_) => Vec::new(),
    }
}

fn paint_all(set: &AutoTileSet, m: &mut TileMapAsset) -> Vec<TileChange> {
    let all: Vec<(u32, u32)> = (0..m.height)
        .flat_map(|y| (0..m.width).map(move |x| (x, y)))
        .collect();
    set.bind(m)
        .expect("binds")
        .paint(m, 0, TerrainBrush::Terrain(0), &all)
        .expect("paints")
}

/// Catches the off-map neighbour policy being applied the wrong way round on a map with no
/// interior: a lone cell is surrounded by the border on all eight sides.
#[test]
fn a_one_cell_map_sees_only_the_border() {
    for (border, edge, blob) in [("Same", 15, 46), ("Empty", 0, 0)] {
        let mut m = map(1, 1);
        paint_all(&edge16(border), &mut m);
        assert_eq!(cells(&m), vec![edge + 1], "Edge16 / {border}");
        let mut m = map(1, 1);
        paint_all(&blob47(border), &mut m);
        assert_eq!(cells(&m), vec![blob + 1], "Blob47 / {border}");
    }
}

/// Catches corner bits being kept when an adjacent edge is off-map-empty, or dropped when the
/// border counts as terrain: a full 2x2 block with `Border::Empty` has a different mask in each
/// corner cell, derived here by hand.
#[test]
fn blob_corners_on_the_map_edge_follow_the_border_mode() {
    // N = 1, NE = 2, E = 4, SE = 8, S = 16, SW = 32, W = 64, NW = 128.
    let mut m = map(2, 2);
    paint_all(&blob47("Empty"), &mut m);
    let expect = [
        4 | 8 | 16,   // top-left: E, SE, S
        16 | 32 | 64, // top-right: S, SW, W
        1 | 2 | 4,    // bottom-left: N, NE, E
        1 | 64 | 128, // bottom-right: N, W, NW
    ];
    let want: Vec<u32> = expect.iter().map(|&mask| blob47_index(mask) as u32 + 1).collect();
    assert_eq!(cells(&m), want);
    // All four are distinct tiles.
    let mut unique = want.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 4);

    let mut m = map(2, 2);
    paint_all(&blob47("Same"), &mut m);
    assert_eq!(
        cells(&m),
        vec![47; 4],
        "off-map counts as terrain: every cell is the full tile"
    );
}

/// Catches rule order being ignored: the first matching rule wins even when a later rule is more
/// specific, so reversing the list must change the picture.
#[test]
fn the_first_matching_rule_wins() {
    let catch_all = r#"(pattern: "??? / ?#? / ???", tile: 30)"#;
    let full = r####"(pattern: "### / ### / ###", tile: 31)"####;
    let block = |first: &str, second: &str| {
        let set = rules(
            "Empty",
            &format!(r#"(name: "t", layout: Rules([{first}, {second}]))"#),
        );
        let mut m = map(3, 3);
        paint_all(&set, &mut m);
        cells(&m)
    };
    // Catch-all first: everything is tile 30 (gid 31).
    assert_eq!(block(catch_all, full), vec![31; 9]);
    // Specific first: only the centre has all eight neighbours.
    let mut want = vec![31; 9];
    want[4] = 32;
    // The catch-all tile is 30 (gid 31) and the full tile 31 (gid 32).
    assert_eq!(block(full, catch_all), want);
}

/// Catches the no-match case picking the wrong default: without a `fallback` the first rule's tile
/// is used; with one, the fallback.
#[test]
fn an_unmatched_cell_uses_the_fallback_or_else_the_first_rule_tile() {
    let two = r#"(pattern: "??? / #?? / ???", tile: 40), (pattern: "??? / ??# / ???", tile: 41)"#;
    let lone = |terrain: &str| {
        let set = rules("Empty", terrain);
        let mut m = map(3, 3);
        set.bind(&m)
            .expect("binds")
            .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(1, 1)])
            .expect("paints");
        cells(&m)[4]
    };
    assert_eq!(lone(&format!(r#"(name: "t", layout: Rules([{two}]))"#)), 41);
    assert_eq!(
        lone(&format!(
            r#"(name: "t", layout: Rules([{two}]), fallback: Some(50))"#
        )),
        51
    );
}

/// Catches the three rule flip flags not all reaching the cell, and a flagged cell not being
/// recognised as a terrain member afterwards (re-resolving it must be a no-op).
#[test]
fn rule_flags_are_written_and_flagged_cells_stay_members() {
    let set = rules(
        "Empty",
        r#"(name: "t", layout: Rules([(pattern: "??? / ?#? / ???", tile: 5, flip_x: true, diagonal: true)]))"#,
    );
    let mut m = map(2, 1);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0), (1, 0)])
        .expect("paints");
    let want = 6 | FLIP_X_BIT | FLIP_DIAGONAL_BIT;
    assert_eq!(cells(&m), vec![want, want]);
    assert_eq!(bound.terrain_of(want), Some(0));
    assert!(bound.apply_layer(&mut m, 0).expect("applies").is_empty());
}

/// Catches `terrain_of` treating flag bits, the empty gid or another tileset's gids as members.
#[test]
fn terrain_of_strips_flags_and_rejects_foreign_gids() {
    let set = edge16("Same");
    let m = map(1, 1);
    let bound = set.bind(&m).expect("binds");
    assert_eq!(bound.terrain_of(1), Some(0));
    assert_eq!(bound.terrain_of(1 | FLIP_X_BIT), Some(0));
    assert_eq!(bound.terrain_of(16), Some(0), "local 15 is the last Edge16 tile");
    assert_eq!(bound.terrain_of(17), None, "local 16 is not a member");
    assert_eq!(bound.terrain_of(0), None);
    assert_eq!(
        bound.terrain_of(FLIP_X_BIT),
        None,
        "a flag-only value is an empty cell"
    );
    assert_eq!(bound.terrain_of(300), None, "another tileset");
    assert_eq!(bound.terrain_of(u32::MAX), None);
}

/// Catches the change list being unsorted, missing a re-resolved neighbour, reporting unchanged
/// cells, or recording a wrong before/after pair.
#[test]
fn paint_reports_exact_sorted_changes_and_is_idempotent() {
    let set = edge16("Empty");
    let mut m = map(3, 1);
    let bound = set.bind(&m).expect("binds");
    let first = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(1, 0)])
        .expect("paints");
    assert_eq!(
        first,
        vec![TileChange {
            x: 1,
            y: 0,
            before: 0,
            after: 1
        }]
    );
    // Painting the left cell joins it to the middle one: (0,0) gets E (mask 2 -> gid 3) and the
    // middle cell gets W (mask 8 -> gid 9). Listed by row, then column.
    let second = bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
        .expect("paints");
    assert_eq!(
        second,
        vec![
            TileChange {
                x: 0,
                y: 0,
                before: 0,
                after: 3
            },
            TileChange {
                x: 1,
                y: 0,
                before: 1,
                after: 9
            },
        ]
    );
    assert_eq!(cells(&m), vec![3, 9, 0]);
    // Painting the same cell again changes nothing.
    assert!(
        bound
            .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
            .expect("paints")
            .is_empty()
    );
    // Duplicate cells in one stroke are the same as listing them once.
    let mut a = map(3, 1);
    let mut b = map(3, 1);
    bound
        .paint(
            &mut a,
            0,
            TerrainBrush::Terrain(0),
            &[(0, 0), (0, 0), (1, 0), (1, 0)],
        )
        .expect("paints");
    bound
        .paint(&mut b, 0, TerrainBrush::Terrain(0), &[(0, 0), (1, 0)])
        .expect("paints");
    assert_eq!(cells(&a), cells(&b));
    assert_eq!(cells(&a), vec![3, 9, 0]);
}

/// Catches erasing leaving the neighbours with edges for a cell that is gone.
#[test]
fn erasing_a_cell_re_resolves_both_neighbours() {
    let set = edge16("Empty");
    let mut m = map(3, 1);
    let bound = set.bind(&m).expect("binds");
    paint_all(&set, &mut m);
    assert_eq!(cells(&m), vec![3, 11, 9], "E, E|W and W");
    let changes = bound
        .paint(&mut m, 0, TerrainBrush::Erase, &[(1, 0)])
        .expect("erases");
    assert_eq!(cells(&m), vec![1, 0, 1]);
    assert_eq!(
        changes,
        vec![
            TileChange {
                x: 0,
                y: 0,
                before: 3,
                after: 1
            },
            TileChange {
                x: 1,
                y: 0,
                before: 11,
                after: 0
            },
            TileChange {
                x: 2,
                y: 0,
                before: 9,
                after: 1
            },
        ]
    );
}

/// Catches painting beside a foreign (props) tile rewriting it: only terrain members are
/// re-resolved, and a foreign neighbour counts as "other".
#[test]
fn foreign_tiles_next_to_a_stroke_are_left_alone() {
    let set = edge16("Empty");
    let mut m = map(3, 1);
    assert!(m.set_tile(0, 2, 0, 301));
    set.bind(&m)
        .expect("binds")
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0), (1, 0)])
        .expect("paints");
    // Cell 1 sees a prop to its east: only W is set (8 -> gid 9).
    assert_eq!(cells(&m), vec![3, 9, 301]);
}

/// Catches hand-placed `members` not counting as terrain for their neighbours, or not being
/// resolved themselves.
#[test]
fn extra_members_join_the_terrain_and_get_resolved() {
    let set = rules(
        "Empty",
        &format!(
            r#"(name: "wall", layout: Edge16({}), members: [200])"#,
            ids(0, 16)
        ),
    );
    let mut m = map(3, 1);
    // wall, hand-placed decoration (a member), wall.
    for (x, gid) in [(0, 1), (1, 201), (2, 1)] {
        assert!(m.set_tile(0, x, 0, gid));
    }
    set.bind(&m)
        .expect("binds")
        .apply_layer(&mut m, 0)
        .expect("applies");
    // The decoration is replaced by the layout tile with both neighbours: E|W = 10 -> gid 11.
    assert_eq!(cells(&m), vec![3, 11, 9]);
}

/// Catches a variant keeping the rule's flip flags (it replaces the tile and its flips) and the
/// resulting tile not being a member.
#[test]
fn a_variant_replaces_the_tile_and_its_flips() {
    let set = rules(
        "Empty",
        r#"(name: "t", layout: Rules([(pattern: "??? / ?#? / ???", tile: 5, flip_x: true)]),
            variants: {5: [(tile: 7, diagonal: true)]})"#,
    );
    let mut m = map(1, 1);
    set.bind(&m)
        .expect("binds")
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
        .expect("paints");
    assert_eq!(cells(&m), vec![8 | FLIP_DIAGONAL_BIT]);
}

/// Catches lookups outside the map or on a non-tile layer returning a tile instead of `None`.
#[test]
fn lookups_outside_the_map_or_on_object_layers_find_nothing() {
    let set = edge16("Same");
    let mut m = map(2, 2);
    let bound = set.bind(&m).expect("binds");
    bound
        .paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0)])
        .expect("paints");
    assert!(bound.resolve(&m, 0, 0, 0).is_some());
    assert_eq!(bound.resolve(&m, 0, 2, 0), None);
    assert_eq!(bound.resolve(&m, 0, 0, 2), None);
    assert_eq!(bound.resolve(&m, 0, 1, 1), None, "an empty cell has no terrain");
    assert_eq!(bound.resolve(&m, 1, 0, 0), None, "layer 1 is an object layer");
    assert_eq!(bound.terrain_at(&m, 1, 0, 0), None);
    assert_eq!(bound.terrain_at(&m, 7, 0, 0), None);
    assert_eq!(bound.terrain_at(&m, 0, 0, 0), Some(0));
}

/// Catches a border mode that does not reach pattern rules: a 3x3 "all neighbours" pattern matches a
/// lone cell only when off-map counts as terrain.
#[test]
fn pattern_rules_honour_the_border_mode() {
    let terrain = r####"(name: "t", layout: Rules([(pattern: "### / ### / ###", tile: 9), (pattern: "??? / ?#? / ???", tile: 3)]))"####;
    for (border, want) in [("Same", 10), ("Empty", 4)] {
        let mut m = map(1, 1);
        paint_all(&rules(border, terrain), &mut m);
        assert_eq!(cells(&m), vec![want], "{border}");
    }
}

/// Catches binding accepting a tile id equal to the tileset's tile count (one past the end).
#[test]
fn binding_checks_tile_ids_against_the_tileset_size() {
    let layout = |first: u32| {
        rules(
            "Same",
            &format!(r#"(name: "t", layout: Edge16({}))"#, ids(first, 16)),
        )
    };
    let m = map(1, 1);
    assert!(layout(240).bind(&m).is_ok(), "ids 240..=255 fit a 256-tile set");
    let err = layout(241).bind(&m).expect_err("id 256 does not exist");
    assert!(matches!(err, AutoTileError::Bind(_)), "{err}");
}

/// Catches validation limits drifting: names, pattern length and variant weights are checked at
/// exactly their documented sizes.
#[test]
fn validation_limits_hold_at_their_exact_boundaries() {
    let parse = |terrains: String, tileset: &str| {
        AutoTileSet::from_ron(&format!(
            r#"(version: {AUTOTILE_FORMAT_VERSION}, tileset: "{tileset}", terrains: [{terrains}])"#
        ))
    };
    let edge = |name: &str| format!(r#"(name: "{name}", layout: Edge16({}))"#, ids(0, 16));
    assert!(parse(edge(&"n".repeat(256)), "terrain").is_ok());
    assert!(matches!(
        parse(edge(&"n".repeat(257)), "terrain"),
        Err(AutoTileError::Invalid(_))
    ));
    assert!(parse(edge("a"), &"s".repeat(256)).is_ok());
    assert!(matches!(
        parse(edge("a"), &"s".repeat(257)),
        Err(AutoTileError::Invalid(_))
    ));
    assert!(matches!(
        parse(edge(""), "terrain"),
        Err(AutoTileError::Invalid(_))
    ));

    let weight = |w: u32| {
        parse(
            format!(
                r#"(name: "t", layout: Edge16({}), variants: {{0: [(tile: 20, weight: {w})]}})"#,
                ids(0, 16)
            ),
            "terrain",
        )
    };
    assert!(weight(65_535).is_ok());
    assert!(matches!(weight(65_536), Err(AutoTileError::Invalid(_))));
    assert!(matches!(weight(0), Err(AutoTileError::Invalid(_))));

    // 9 cells + 55 spaces = 64 bytes passes; one more byte fails.
    let pattern = |pad: usize| {
        parse(
            format!(
                r#"(name: "t", layout: Rules([(pattern: "{}#{}{}", tile: 1)]))"#,
                "?".repeat(4),
                "?".repeat(4),
                " ".repeat(pad)
            ),
            "terrain",
        )
    };
    assert!(pattern(55).is_ok());
    assert!(matches!(pattern(56), Err(AutoTileError::Invalid(_))));
}

/// Catches an agent-visible error code changing: the codes are a stable protocol.
#[test]
fn every_error_class_has_its_stable_agent_code() {
    for (err, code) in [
        (
            AutoTileError::Version {
                found: 9,
                expected: 1,
            },
            "tile_bad_rules",
        ),
        (AutoTileError::Parse(String::new()), "tile_bad_rules"),
        (AutoTileError::Invalid(String::new()), "tile_bad_rules"),
        (AutoTileError::Migration(String::new()), "tile_bad_rules"),
        (AutoTileError::Bind(String::new()), "tile_bind_failed"),
        (AutoTileError::NotTileLayer(1), "tile_not_tile_layer"),
        (AutoTileError::OutsideMap { x: 1, y: 2 }, "tile_outside_map"),
        (AutoTileError::UnknownTerrain(3), "tile_unknown_terrain"),
    ] {
        assert_eq!(err.code(), code, "{err}");
    }
}

/// Catches a painted-cell check that lets a stroke through that partly misses the layer kind or
/// map, leaving a half-applied stroke.
#[test]
fn a_rejected_stroke_leaves_the_map_untouched() {
    let set = edge16("Empty");
    let mut m = map(3, 3);
    let before = cells(&m);
    let bound = set.bind(&m).expect("binds");
    assert_eq!(
        bound.paint(&mut m, 0, TerrainBrush::Terrain(0), &[(0, 0), (1, 1), (3, 3)]),
        Err(AutoTileError::OutsideMap { x: 3, y: 3 })
    );
    assert_eq!(
        bound.paint(&mut m, 1, TerrainBrush::Terrain(0), &[(0, 0)]),
        Err(AutoTileError::NotTileLayer(1))
    );
    assert_eq!(
        bound.paint(&mut m, 0, TerrainBrush::Terrain(1), &[(0, 0)]),
        Err(AutoTileError::UnknownTerrain(1))
    );
    assert_eq!(cells(&m), before);
}
