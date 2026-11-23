// SPDX-License-Identifier: MPL-2.0
use super::*;
use regia_2d::autotile::Layout;

/// Corner wang tiles: id `(mask * 7) % 16` shows exactly the corners in `mask` (TL 1, TR 2,
/// BR 4, BL 8) in color 1.
fn corner_tiles(range: std::ops::RangeInclusive<u32>) -> Vec<(u32, [u8; 8])> {
    let mut tiles: Vec<(u32, [u8; 8])> = range
        .map(|mask| {
            let on = |bit: u32| u8::from(mask & bit != 0);
            ((mask * 7) % 16, [0, on(2), 0, on(4), 0, on(8), 0, on(1)])
        })
        .collect();
    tiles.sort_unstable();
    tiles
}

fn corner_set(tiles: Vec<(u32, [u8; 8])>) -> WangData {
    WangData {
        name: "Ground".into(),
        kind: WangKind::Corner,
        colors: vec!["Grass".into()],
        tiles,
    }
}

fn layout_of(imported: &ImportedAutoTile, terrain: usize) -> &Layout {
    &imported.set.terrains[terrain].layout
}

#[test]
fn corner_set_maps_every_mask_to_its_tile() {
    let imported =
        autotile_from_wang_sets("t", &[corner_set(corner_tiles(1..=15))], Vec::new()).expect("rules");
    let Layout::WangCorner(tiles) = layout_of(&imported, 0) else {
        panic!("a corner set gives WangCorner");
    };
    for mask in 1..16u32 {
        assert_eq!(tiles[mask as usize], (mask * 7) % 16, "mask {mask}");
    }
    // Mask 0 (no corner inside) has no tile of its own: the nearest mask (1) stands in.
    assert_eq!(tiles[0], 7);
    assert!(
        imported.warnings.iter().any(|w| w.contains("nearest")),
        "{:?}",
        imported.warnings
    );
    assert_eq!(imported.set.tileset, "t");
}

#[test]
fn the_lowest_tile_id_wins_for_a_shared_mask() {
    let mut tiles = corner_tiles(1..=15);
    tiles.push((99, [0, 0, 0, 0, 0, 0, 0, 1]));
    tiles.sort_unstable();
    let imported = autotile_from_wang_sets("t", &[corner_set(tiles)], Vec::new()).expect("rules");
    // Mask 1 now has two tiles; the lower id (7 for mask 1 vs 99) wins.
    let Layout::WangCorner(l) = layout_of(&imported, 0) else {
        panic!()
    };
    assert_eq!(l[1], 7);
}

#[test]
fn transition_and_empty_tiles_belong_to_no_terrain() {
    let mut tiles = corner_tiles(1..=15);
    tiles.push((40, [0, 2, 0, 0, 0, 0, 0, 1])); // grass next to color 2: a transition
    tiles.push((41, [0; 8])); // empty
    tiles.sort_unstable();
    let imported = autotile_from_wang_sets("t", &[corner_set(tiles)], Vec::new()).expect("rules");
    let members = imported.set.terrains[0].member_tiles();
    assert!(!members.contains(&40) && !members.contains(&41), "{members:?}");
}

#[test]
fn a_color_without_tiles_is_skipped_with_a_warning() {
    let mut data = corner_set(corner_tiles(1..=15));
    data.colors.push("Rock".into());
    let imported = autotile_from_wang_sets("t", &[data], Vec::new()).expect("rules");
    assert_eq!(imported.set.terrains.len(), 1);
    assert!(
        imported.warnings.iter().any(|w| w.contains("Rock")),
        "{:?}",
        imported.warnings
    );
}

#[test]
fn nothing_usable_gives_none() {
    let data = WangData {
        name: "x".into(),
        kind: WangKind::Edge,
        colors: vec!["a".into()],
        tiles: vec![],
    };
    assert!(autotile_from_wang_sets("t", &[data], Vec::new()).is_none());
}

#[test]
fn colliding_terrain_names_get_the_set_prefix() {
    let a = corner_set(corner_tiles(1..=15));
    let mut b = corner_set(
        corner_tiles(1..=15)
            .into_iter()
            .map(|(t, w)| (t + 16, w))
            .collect(),
    );
    b.name = "Other".into();
    let imported = autotile_from_wang_sets("t", &[a, b], Vec::new()).expect("rules");
    let names: Vec<&str> = imported.set.terrains.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Grass", "Other/Grass"]);
}

#[test]
fn mixed_set_gives_a_blob47_layout() {
    // Every reduced mask gets a tile whose eight slots are exactly the mask bits.
    let tiles: Vec<(u32, [u8; 8])> = regia_2d::autotile::blob47_masks()
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, &m)| (i as u32, std::array::from_fn(|bit| u8::from(m & (1 << bit) != 0))))
        .collect();
    let data = WangData {
        name: "Blob".into(),
        kind: WangKind::Mixed,
        colors: vec!["Stone".into()],
        tiles,
    };
    let imported = autotile_from_wang_sets("t", &[data], Vec::new()).expect("rules");
    let Layout::Blob47(l) = layout_of(&imported, 0) else {
        panic!()
    };
    assert_eq!(l.len(), 47);
    assert_eq!(l[46], 46);
    assert_eq!(l[10], 10);
    assert_eq!(l[0], 1, "mask 0 stands in with the nearest covered mask's tile");
}

/// A legacy terrain corner of `i64::MAX` (a hostile `.tsx` `terrain="9223372036854775807,0,0,0"`)
/// is no color, not an add overflow (a panic in builds with overflow checks: tests, dev tools).
#[test]
fn a_huge_legacy_terrain_corner_is_no_color_not_an_overflow() {
    let data = legacy_terrains_to_wang(vec!["a".into()], &[(0, [i64::MAX, 0, -1, i64::MIN])]);
    // Slots: top, TR, right, BR, bottom, BL, left, TL.
    assert_eq!(data.tiles, vec![(0, [0, 1, 0, 0, 0, 0, 0, 0])]);
}
