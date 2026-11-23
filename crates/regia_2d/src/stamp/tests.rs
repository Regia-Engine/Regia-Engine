// SPDX-License-Identifier: MPL-2.0
use std::collections::BTreeMap;

use super::*;
use crate::tilemap::{Orientation, TileLayer, TilesetDef};

const OPS: [StampOp; 5] = [
    StampOp::RotateCw,
    StampOp::RotateCcw,
    StampOp::Rotate180,
    StampOp::FlipX,
    StampOp::FlipY,
];

type Image = [[u8; 3]; 3];

/// An asymmetric picture: every symmetry changes it.
const PICTURE: Image = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];

fn transpose(i: &Image) -> Image {
    std::array::from_fn(|y| std::array::from_fn(|x| i[x][y]))
}
fn mirror_x(i: &Image) -> Image {
    std::array::from_fn(|y| std::array::from_fn(|x| i[y][2 - x]))
}
fn mirror_y(i: &Image) -> Image {
    std::array::from_fn(|y| std::array::from_fn(|x| i[2 - y][x]))
}
fn rotate_cw(i: &Image) -> Image {
    // Clockwise: the left column becomes the top row, read bottom to top.
    std::array::from_fn(|y| std::array::from_fn(|x| i[2 - x][y]))
}

/// What a tile with `raw`'s flip bits shows (Tiled: diagonal first, then horizontal, then vertical).
fn show(raw: u32) -> Image {
    let mut i = PICTURE;
    if raw & FLIP_DIAGONAL_BIT != 0 {
        i = transpose(&i);
    }
    if raw & FLIP_X_BIT != 0 {
        i = mirror_x(&i);
    }
    if raw & FLIP_Y_BIT != 0 {
        i = mirror_y(&i);
    }
    i
}

fn apply(op: StampOp, i: &Image) -> Image {
    match op {
        StampOp::RotateCw => rotate_cw(i),
        StampOp::RotateCcw => rotate_cw(&rotate_cw(&rotate_cw(i))),
        StampOp::Rotate180 => rotate_cw(&rotate_cw(i)),
        StampOp::FlipX => mirror_x(i),
        StampOp::FlipY => mirror_y(i),
    }
}

#[test]
fn flip_bits_turn_with_the_stamp_for_every_combination() {
    for bits in 0..8u32 {
        let raw = 5
            | (if bits & 1 != 0 { FLIP_X_BIT } else { 0 })
            | (if bits & 2 != 0 { FLIP_Y_BIT } else { 0 })
            | (if bits & 4 != 0 { FLIP_DIAGONAL_BIT } else { 0 });
        for op in OPS {
            let out = transform_gid(raw, op);
            assert_eq!(
                out & !(FLIP_X_BIT | FLIP_Y_BIT | FLIP_DIAGONAL_BIT),
                5,
                "the id is kept"
            );
            assert_eq!(show(out), apply(op, &show(raw)), "{op:?} on flags {bits:03b}");
        }
    }
    assert_eq!(transform_gid(0, StampOp::RotateCw), 0, "empty stays empty");
}

fn sample() -> Stamp {
    // 3 columns x 2 rows, ids 1..=6 with distinct flips on two of them.
    Stamp::new(3, 2, vec![1, 2 | FLIP_X_BIT, 3, 4 | FLIP_DIAGONAL_BIT, 0, 6]).expect("valid")
}

#[test]
fn rotations_and_flips_compose() {
    let s = sample();
    let cw4 = (0..4).fold(s.clone(), |acc, _| acc.transformed(StampOp::RotateCw));
    assert_eq!(cw4, s);
    assert_eq!(
        s.transformed(StampOp::RotateCw).transformed(StampOp::RotateCcw),
        s
    );
    assert_eq!(
        s.transformed(StampOp::RotateCw).transformed(StampOp::RotateCw),
        s.transformed(StampOp::Rotate180)
    );
    assert_eq!(s.transformed(StampOp::FlipX).transformed(StampOp::FlipX), s);
    assert_eq!(s.transformed(StampOp::FlipY).transformed(StampOp::FlipY), s);
    assert_eq!(
        s.transformed(StampOp::FlipX).transformed(StampOp::FlipY),
        s.transformed(StampOp::Rotate180)
    );
}

#[test]
fn rotate_cw_moves_cells_like_a_quarter_turn() {
    // Plain tiles: [1 2 3 / 4 5 6] rotated clockwise is [4 1 / 5 2 / 6 3].
    let s = Stamp::new(3, 2, vec![1, 2, 3, 4, 5, 6]).expect("valid");
    let r = s.transformed(StampOp::RotateCw);
    assert_eq!((r.width, r.height), (2, 3));
    let ids: Vec<u32> = r.cells.iter().map(|c| c & GID_ID).collect();
    assert_eq!(ids, vec![4, 1, 5, 2, 6, 3]);
    let ccw = s.transformed(StampOp::RotateCcw);
    let ids: Vec<u32> = ccw.cells.iter().map(|c| c & GID_ID).collect();
    assert_eq!(ids, vec![3, 6, 2, 5, 1, 4]);
}

const GID_ID: u32 = crate::tilemap::GID_MASK;

fn map(width: u32, height: u32) -> TileMapAsset {
    TileMapAsset {
        version: crate::tilemap::TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width,
        height,
        tile_width: 8,
        tile_height: 8,
        background: None,
        tilesets: vec![TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 64,
            image_height: 64,
            tile_width: 8,
            tile_height: 8,
            columns: 8,
            tile_count: 64,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }],
        layers: vec![MapLayer::Tiles(TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; (width * height) as usize],
            properties: BTreeMap::new(),
        })],
        properties: BTreeMap::new(),
    }
}

fn layer(map: &TileMapAsset) -> &[u32] {
    match &map.layers[0] {
        MapLayer::Tiles(t) => &t.data,
        MapLayer::Objects(_) => &[],
    }
}

#[test]
fn copy_then_paste_round_trips_and_reports_changes() {
    let mut m = map(5, 4);
    let s = sample();
    let changes = s.paste(&mut m, 0, (1, 1), true).expect("pastes");
    // The empty cell (1, 1) of the stamp is skipped, so five cells changed.
    assert_eq!(changes.len(), 5);
    assert_eq!(
        changes[0],
        crate::autotile::TileChange {
            x: 1,
            y: 1,
            before: 0,
            after: 1
        }
    );
    let copied = Stamp::copy(&m, 0, (3, 2), (1, 1)).expect("copies (corners in any order)");
    assert_eq!(
        copied.cells,
        vec![1, 2 | FLIP_X_BIT, 3, 4 | FLIP_DIAGONAL_BIT, 0, 6]
    );
    // Undo: writing the `before` values back restores the empty map.
    crate::autotile::revert_changes(&mut m, 0, &changes).expect("reverts");
    assert!(layer(&m).iter().all(|&c| c == 0));
}

#[test]
fn paste_without_skip_empty_erases_and_clips_at_the_edges() {
    let mut m = map(4, 3);
    for x in 0..4 {
        m.set_tile(0, x, 0, 9);
    }
    let s = Stamp::new(2, 2, vec![1, 0, 2, 3]).expect("valid");
    // Hanging over the top-left corner: only the stamp's bottom-right cell (3) lands, on (0, 0).
    let changes = s.paste(&mut m, 0, (-1, -1), false).expect("pastes");
    assert_eq!(changes.len(), 1);
    assert_eq!(layer(&m)[0], 3);
    // Not skipping empties: the stamp's empty cell overwrites a tile.
    let s = Stamp::new(2, 1, vec![0, 7]).expect("valid");
    s.paste(&mut m, 0, (1, 0), false).expect("pastes");
    assert_eq!(&layer(&m)[0..4], &[3, 0, 7, 9]);
    // Entirely outside: nothing written, not an error.
    assert!(s.paste(&mut m, 0, (50, 50), true).expect("pastes").is_empty());
}

#[test]
fn errors_have_codes_and_nothing_is_half_written() {
    let mut m = map(3, 3);
    assert_eq!(
        Stamp::copy(&m, 0, (0, 0), (3, 0)).expect_err("outside").code(),
        "tile_outside_map"
    );
    assert_eq!(
        Stamp::copy(&m, 5, (0, 0), (1, 1)).expect_err("layer").code(),
        "tile_not_tile_layer"
    );
    assert_eq!(
        Stamp::new(0, 1, vec![]).expect_err("zero").code(),
        "tile_bad_stamp"
    );
    assert_eq!(
        Stamp::new(2, 2, vec![1]).expect_err("count").code(),
        "tile_bad_stamp"
    );
    let bad = Stamp::new(2, 1, vec![1, 5000]).expect("valid shape");
    assert_eq!(
        bad.paste(&mut m, 0, (0, 0), true).expect_err("gid").code(),
        "tile_bad_gid"
    );
    assert!(
        layer(&m).iter().all(|&c| c == 0),
        "the valid cell was not written either"
    );
    let ok = Stamp::new(1, 1, vec![1]).expect("valid");
    assert_eq!(
        ok.paste(&mut m, 9, (0, 0), true).expect_err("layer").code(),
        "tile_not_tile_layer"
    );
}

#[test]
fn op_names_round_trip() {
    for op in OPS {
        assert_eq!(StampOp::parse(op.name()), Some(op));
    }
    assert_eq!(StampOp::parse("spin"), None);
}

#[test]
fn copy_allows_past_the_paint_cap_and_rejects_past_the_stamp_cap() {
    // 257 × 256 = 65 792 cells: above the 65 536 paint/paste cap, under `1 << 20`.
    let wide = map(257, 256);
    let copied = Stamp::copy(&wide, 0, (0, 0), (256, 255)).expect("copies");
    assert_eq!(copied.cells.len(), 65_792);
    let mut huge = map(1, 1);
    huge.width = 1025;
    huge.height = 1024;
    let err = Stamp::copy(&huge, 0, (0, 0), (1024, 1023)).expect_err("over the stamp cap");
    assert_eq!(err.code(), "tile_bad_stamp");
}
