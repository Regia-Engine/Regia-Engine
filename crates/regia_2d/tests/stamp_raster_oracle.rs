// SPDX-License-Identifier: MPL-2.0
//! Independent raster oracle for composed stamp transforms and clipped pastes.

use std::collections::BTreeMap;

use regia_2d::{
    autotile::{TileChange, revert_changes},
    stamp::{Stamp, StampOp},
    tilemap::{
        FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, Orientation, TILEMAP_FORMAT_VERSION, TileLayer,
        TileMapAsset, TilesetDef,
    },
};

const OPS: [StampOp; 5] = [
    StampOp::RotateCw,
    StampOp::RotateCcw,
    StampOp::Rotate180,
    StampOp::FlipX,
    StampOp::FlipY,
];
const PICTURE: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
const FLAGS: [u32; 8] = [
    0,
    FLIP_X_BIT,
    FLIP_Y_BIT,
    FLIP_X_BIT | FLIP_Y_BIT,
    FLIP_DIAGONAL_BIT,
    FLIP_X_BIT | FLIP_DIAGONAL_BIT,
    FLIP_Y_BIT | FLIP_DIAGONAL_BIT,
    FLIP_X_BIT | FLIP_Y_BIT | FLIP_DIAGONAL_BIT,
];

fn picture_for(raw: u32) -> [u8; 9] {
    let mut picture = PICTURE;
    if raw & FLIP_DIAGONAL_BIT != 0 {
        picture = std::array::from_fn(|index| picture[(index % 3) * 3 + index / 3]);
    }
    if raw & FLIP_X_BIT != 0 {
        picture = std::array::from_fn(|index| picture[(index / 3) * 3 + (2 - index % 3)]);
    }
    if raw & FLIP_Y_BIT != 0 {
        picture = std::array::from_fn(|index| picture[(2 - index / 3) * 3 + index % 3]);
    }
    picture
}

fn turned_picture(op: StampOp, picture: [u8; 9]) -> [u8; 9] {
    std::array::from_fn(|index| {
        let (x, y) = ((index % 3) as u8, (index / 3) as u8);
        let (sx, sy) = match op {
            StampOp::RotateCw => (y, 2 - x),
            StampOp::RotateCcw => (2 - y, x),
            StampOp::Rotate180 => (2 - x, 2 - y),
            StampOp::FlipX => (2 - x, y),
            StampOp::FlipY => (x, 2 - y),
        };
        picture[usize::from(sy) * 3 + usize::from(sx)]
    })
}

fn transformed_raw(raw: u32, op: StampOp) -> u32 {
    if raw == 0 {
        return 0;
    }
    let wanted = turned_picture(op, picture_for(raw));
    let flags = FLAGS
        .into_iter()
        .find(|flags| picture_for((raw & 0x0fff_ffff) | flags) == wanted)
        .expect("the eight asymmetric picture orientations are distinct");
    (raw & 0x0fff_ffff) | flags
}

fn transformed_cells(stamp: &Stamp, op: StampOp) -> (u32, u32, Vec<u32>) {
    let (w, h) = (stamp.width, stamp.height);
    let (nw, nh) = match op {
        StampOp::RotateCw | StampOp::RotateCcw => (h, w),
        _ => (w, h),
    };
    let mut cells = vec![0; (nw * nh) as usize];
    for dy in 0..nh {
        for dx in 0..nw {
            let (sx, sy) = match op {
                StampOp::RotateCw => (dy, h - 1 - dx),
                StampOp::RotateCcw => (w - 1 - dy, dx),
                StampOp::Rotate180 => (w - 1 - dx, h - 1 - dy),
                StampOp::FlipX => (w - 1 - dx, dy),
                StampOp::FlipY => (dx, h - 1 - dy),
            };
            cells[(dy * nw + dx) as usize] = transformed_raw(stamp.cells[(sy * w + sx) as usize], op);
        }
    }
    (nw, nh, cells)
}

fn test_map() -> TileMapAsset {
    let width = 3;
    let height = 2;
    let data = (0..width * height)
        .map(|index| {
            if index % 3 == 1 {
                0
            } else {
                (index + 20) | FLAGS[index as usize % 8]
            }
        })
        .collect();
    TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width,
        height,
        tile_width: 8,
        tile_height: 8,
        background: None,
        tilesets: vec![TilesetDef {
            name: "oracle".into(),
            first_gid: 1,
            image: "oracle.png".into(),
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
            name: "ground".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0; 2],
            data,
            properties: BTreeMap::new(),
        })],
        properties: BTreeMap::new(),
    }
}

fn map_cells(map: &TileMapAsset) -> &[u32] {
    match &map.layers[0] {
        MapLayer::Tiles(layer) => &layer.data,
        MapLayer::Objects(_) => unreachable!("test map has a tile layer"),
    }
}

struct RasterStamp<'a> {
    width: u32,
    height: u32,
    cells: &'a [u32],
}

fn raster_oracle(
    before: &[u32],
    map: &TileMapAsset,
    stamp: RasterStamp<'_>,
    at: (i64, i64),
    skip_empty: bool,
) -> (Vec<u32>, Vec<TileChange>) {
    let mut after = before.to_vec();
    let mut changes = Vec::new();
    for y in 0..map.height {
        for x in 0..map.width {
            let sx = i64::from(x) - at.0;
            let sy = i64::from(y) - at.1;
            if sx < 0 || sy < 0 || sx >= i64::from(stamp.width) || sy >= i64::from(stamp.height) {
                continue;
            }
            let raw = stamp.cells[(sy as u32 * stamp.width + sx as u32) as usize];
            if skip_empty && raw == 0 {
                continue;
            }
            let index = (y * map.width + x) as usize;
            let old = after[index];
            if old != raw {
                after[index] = raw;
                changes.push(TileChange {
                    x,
                    y,
                    before: old,
                    after: raw,
                });
            }
        }
    }
    (after, changes)
}

fn all_stamps() -> Vec<Stamp> {
    [(3, 2), (1, 4), (4, 1), (3, 3)]
        .into_iter()
        .map(|(width, height)| {
            let cells = (0..width * height)
                .map(|index| {
                    if (width, height) == (3, 3) && index == 4 {
                        0
                    } else {
                        let flag_index = if (width, height) == (3, 3) && index > 4 {
                            index - 1
                        } else {
                            index
                        };
                        (1 + index % 63) | FLAGS[flag_index as usize % FLAGS.len()]
                    }
                })
                .collect();
            Stamp::new(width, height, cells).expect("bounded oracle stamp")
        })
        .collect()
}

#[test]
fn transformed_stamp_pastes_match_independent_full_map_raster_for_every_edge_placement() {
    let initial = test_map();
    for stamp in all_stamps() {
        for op in OPS {
            let (width, height, cells) = transformed_cells(&stamp, op);
            let transformed = stamp.transformed(op);
            assert_eq!((transformed.width, transformed.height), (width, height));
            for y in -(i64::from(height))..=i64::from(initial.height) {
                for x in -(i64::from(width))..=i64::from(initial.width) {
                    for skip_empty in [false, true] {
                        let mut actual = initial.clone();
                        let original = map_cells(&actual).to_vec();
                        let (expected_map, expected_changes) = raster_oracle(
                            &original,
                            &initial,
                            RasterStamp {
                                width,
                                height,
                                cells: &cells,
                            },
                            (x, y),
                            skip_empty,
                        );
                        let changes = transformed
                            .paste(&mut actual, 0, (x, y), skip_empty)
                            .expect("all generated ids belong to the test tileset");
                        assert_eq!(
                            map_cells(&actual),
                            expected_map,
                            "{width}x{height} {op:?} at ({x},{y}), skip={skip_empty}"
                        );
                        assert_eq!(
                            changes, expected_changes,
                            "change records {op:?} at ({x},{y}), skip={skip_empty}"
                        );
                        let mut coordinates = changes.iter().map(|c| (c.y, c.x)).collect::<Vec<_>>();
                        coordinates.sort_unstable();
                        coordinates.dedup();
                        assert_eq!(coordinates.len(), changes.len(), "duplicate change coordinate");
                        assert!(
                            changes
                                .windows(2)
                                .all(|pair| (pair[0].y, pair[0].x) < (pair[1].y, pair[1].x)),
                            "changes are row-major"
                        );
                        revert_changes(&mut actual, 0, &changes).expect("oracle changes are in bounds");
                        assert_eq!(map_cells(&actual), original, "before values restore full map");
                    }
                }
            }
            for at in [
                (-i64::from(width) - 1, 0),
                (i64::from(initial.width) + 1, 0),
                (0, -i64::from(height) - 1),
                (0, i64::from(initial.height) + 1),
            ] {
                for skip_empty in [false, true] {
                    let mut actual = initial.clone();
                    let changes = transformed
                        .paste(&mut actual, 0, at, skip_empty)
                        .expect("fully outside is an empty paste");
                    assert!(changes.is_empty());
                    assert_eq!(map_cells(&actual), map_cells(&initial));
                }
            }
        }
    }
}

#[test]
fn a_visible_invalid_gid_refuses_the_entire_paste_without_partial_writes() {
    let mut map = test_map();
    let before = map.clone();
    let stamp = Stamp::new(2, 1, vec![7, 65]).expect("valid stamp shape");
    let error = stamp
        .paste(&mut map, 0, (0, 0), true)
        .expect_err("gid 65 is outside the tileset");
    assert_eq!(error.code(), "tile_bad_gid");
    assert_eq!(map, before, "the preceding valid visible gid must not be written");
}
