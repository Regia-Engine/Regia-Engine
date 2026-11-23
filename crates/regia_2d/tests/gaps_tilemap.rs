// SPDX-License-Identifier: MPL-2.0
//! Tilemap core behaviour not covered by the unit tests (test-gap audit): rectangle math with
//! margin and spacing, GID resolution at tileset boundaries and gaps, non-square cell mapping,
//! documented defaults, property and object round trips, validation limits, collision with flip
//! flags and corrupt input. CPU only.
//!
//! Maps are written as RON so the tests do not depend on the exact set of struct fields.

use regia_2d::atlas::RgbaImage;
use regia_2d::tilemap::{
    COOKED_TILEMAP_VERSION, CookedTileMap, FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, ObjectShape,
    PropertyValue, TILEMAP_FORMAT_VERSION, TileCollision, TileMapAsset, TileMapError,
};

fn ron(body: &str) -> String {
    format!("(version: {TILEMAP_FORMAT_VERSION}, {body})")
}

/// Two tilesets with a gid gap: `a` holds gids 1..=4, `b` holds gids 10..=11.
const TWO_SETS: &str = r#"tilesets: [
    (name: "a", first_gid: 1, image: "a.png", image_width: 32, image_height: 8,
     tile_width: 8, tile_height: 8, columns: 4, tile_count: 4,
     tiles: [(id: 1, collision: Solid), (id: 2, collision: OneWay)]),
    (name: "b", first_gid: 10, image: "b.png", image_width: 16, image_height: 8,
     tile_width: 8, tile_height: 8, columns: 2, tile_count: 2)]"#;

fn map(width: u32, height: u32, tile_w: u32, tile_h: u32, data: &[u32]) -> TileMapAsset {
    let data = data.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    TileMapAsset::from_ron(&ron(&format!(
        "width: {width}, height: {height}, tile_width: {tile_w}, tile_height: {tile_h}, {TWO_SETS},
         layers: [Tiles((name: \"ground\", data: [{data}]))]"
    )))
    .expect("test map is valid")
}

/// Catches margin and spacing being swapped, dropped, or applied to the wrong axis in the source
/// rectangle of a tile.
#[test]
fn tile_rects_apply_margin_once_and_spacing_between_tiles() {
    let m = TileMapAsset::from_ron(&ron(r#"width: 1, height: 1, tile_width: 8, tile_height: 8,
        tilesets: [(name: "s", first_gid: 1, image: "s.png", image_width: 32, image_height: 32,
                    tile_width: 8, tile_height: 8, columns: 3, tile_count: 9, spacing: 2, margin: 1)],
        layers: [Tiles((name: "g", data: [0]))]"#))
    .expect("valid");
    let set = &m.tilesets[0];
    assert_eq!(set.tile_rect(0), Some([1, 1, 8, 8]));
    assert_eq!(set.tile_rect(1), Some([11, 1, 8, 8]));
    assert_eq!(
        set.tile_rect(3),
        Some([1, 11, 8, 8]),
        "first tile of the second row"
    );
    assert_eq!(set.tile_rect(4), Some([11, 11, 8, 8]));
    assert_eq!(set.tile_rect(8), Some([21, 21, 8, 8]));
    assert_eq!(set.tile_rect(9), None, "one past the last tile");
}

/// Catches an off-by-one at the edges of a tileset's gid range and a gap between tilesets being
/// attributed to the earlier one.
#[test]
fn gids_resolve_exactly_inside_their_tileset_and_nowhere_in_gaps() {
    let m = map(1, 1, 8, 8, &[0]);
    assert_eq!(m.tileset_for(0), None);
    assert_eq!(m.tileset_for(1), Some((0, 0)));
    assert_eq!(m.tileset_for(4), Some((0, 3)));
    for gap in [5, 7, 9] {
        assert_eq!(m.tileset_for(gap), None, "gid {gap} is between the tilesets");
    }
    assert_eq!(m.tileset_for(10), Some((1, 0)));
    assert_eq!(m.tileset_for(11), Some((1, 1)));
    assert_eq!(m.tileset_for(12), None);
    assert_eq!(m.tileset_for(u32::MAX), None);
}

/// Catches `tile_at` accepting the first cell past an edge, reading object layers as tiles, or
/// dropping the flip flags of a resolved tile.
#[test]
fn tile_at_is_bounded_and_keeps_flags() {
    let mut m = map(
        3,
        2,
        8,
        8,
        &[0, 2 | FLIP_X_BIT, 0, 10 | FLIP_Y_BIT | FLIP_DIAGONAL_BIT, 0, 4],
    );
    assert!(m.tile_at(0, 0, 0).is_none(), "gid 0 is empty");
    let t = m.tile_at(0, 1, 0).expect("tile");
    assert_eq!((t.gid, t.tileset, t.local_id), (2, 0, 1));
    assert!(t.flags.flip_x && !t.flags.flip_y && !t.flags.diagonal);
    let t = m.tile_at(0, 0, 1).expect("tile");
    assert_eq!((t.gid, t.tileset, t.local_id), (10, 1, 0));
    assert!(!t.flags.flip_x && t.flags.flip_y && t.flags.diagonal);
    assert!(m.tile_at(0, 3, 0).is_none(), "x == width");
    assert!(m.tile_at(0, 0, 2).is_none(), "y == height");
    assert!(m.tile_at(0, 2, 1).is_some(), "the last cell is inside");
    assert!(m.tile_at(1, 0, 0).is_none(), "no such layer");
    // A gid that falls in the gap between tilesets resolves to nothing instead of the wrong set.
    if let Some(MapLayer::Tiles(layer)) = m.layers.first_mut() {
        layer.data[0] = 6;
    }
    assert!(m.tile_at(0, 0, 0).is_none());
}

/// Catches a write landing in a neighbouring cell or on the wrong row: only the addressed cell may
/// change, including at the last column and row.
#[test]
fn set_tile_changes_exactly_one_cell() {
    let mut m = map(3, 2, 8, 8, &[1, 1, 1, 1, 1, 1]);
    assert!(m.set_tile(0, 2, 1, 3));
    assert!(m.set_tile(0, 0, 1, 4 | FLIP_X_BIT));
    let layer = m.tile_layer(0).expect("tiles");
    assert_eq!(layer.data, vec![1, 1, 1, 4 | FLIP_X_BIT, 1, 3]);
    // Rejected writes leave the data alone.
    assert!(!m.set_tile(0, 3, 0, 1));
    assert!(!m.set_tile(0, 0, 2, 1));
    assert!(!m.set_tile(0, 0, 0, 6), "gid in the gap between tilesets");
    assert_eq!(
        m.tile_layer(0).expect("tiles").data,
        vec![1, 1, 1, 4 | FLIP_X_BIT, 1, 3]
    );
}

/// Catches world<->cell conversion using the tile width for both axes, and cell boundaries
/// resolving to the wrong neighbour. Tiles are 16 x 8 so mixing the axes shows.
#[test]
fn cell_lookup_on_non_square_tiles_round_trips_every_cell() {
    let m = map(3, 2, 16, 8, &[0; 6]);
    assert_eq!(m.pixel_size(), [48, 16]);
    assert_eq!(m.tile_world_rect(2, 0), ([32.0, 8.0], [48.0, 16.0]));
    assert_eq!(m.tile_world_rect(0, 1), ([0.0, 0.0], [16.0, 8.0]));
    assert_eq!(m.cell_at_world([15.99, 0.0]), Some((0, 1)));
    assert_eq!(m.cell_at_world([16.0, 0.0]), Some((1, 1)));
    assert_eq!(m.cell_at_world([16.0, 7.99]), Some((1, 1)));
    assert_eq!(m.cell_at_world([16.0, 8.0]), Some((1, 0)));
    assert_eq!(m.cell_at_world([47.99, 15.99]), Some((2, 0)));
    for outside in [
        [48.0, 0.0],
        [0.0, 16.0],
        [-0.001, 0.0],
        [0.0, -0.001],
        [f32::NAN, 0.0],
    ] {
        assert_eq!(m.cell_at_world(outside), None, "{outside:?}");
    }
    for y in 0..2 {
        for x in 0..3 {
            let ([x0, y0], [x1, y1]) = m.tile_world_rect(x, y);
            let center = [((x0 + x1) / 2.0) as f32, ((y0 + y1) / 2.0) as f32];
            assert_eq!(m.cell_at_world(center), Some((x, y)));
        }
    }
}

/// Catches a changed default for optional layer fields: a layer written without them must be
/// visible, fully opaque and unshifted (files in the wild omit them).
#[test]
fn omitted_layer_fields_take_their_documented_defaults() {
    let m = TileMapAsset::from_ron(&ron(
        r#"width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [],
        layers: [Tiles((name: "g", data: [0])), Objects((name: "o", objects: [(id: 1, position: (1.0, 2.0), shape: Point)]))]"#,
    ))
    .expect("valid");
    let MapLayer::Tiles(t) = &m.layers[0] else {
        panic!("tiles")
    };
    assert!(t.visible);
    assert_eq!(t.opacity, 1.0);
    assert_eq!(t.offset, [0.0, 0.0]);
    assert!(t.properties.is_empty());
    let MapLayer::Objects(o) = &m.layers[1] else {
        panic!("objects")
    };
    assert!(o.visible);
    assert_eq!(o.objects[0].size, [0.0, 0.0]);
    assert_eq!(o.objects[0].rotation, 0.0);
    assert_eq!(m.background, None);
}

/// Catches a property or object shape that does not survive a save and load (lost variant,
/// float rounding, point order).
#[test]
fn properties_objects_and_shapes_survive_a_ron_round_trip() {
    let text = ron(
        r##"width: 1, height: 1, tile_width: 8, tile_height: 8, background: Some("#80ff00ff"),
        tilesets: [],
        properties: {"b": Bool(true), "i": Int(-9007199254740993), "f": Float(0.1), "s": String("hi \"there\""),
                     "c": Color("#ff00ff"), "p": File("a/b.png"), "o": Object(7)},
        layers: [Tiles((name: "g", offset: (3.5, -2.25), opacity: 0.375, visible: false, data: [0])),
                 Objects((name: "o", objects: [
                    (id: 1, name: "spawn", class: "Spawn", position: (1.5, 2.5), size: (3.0, 4.0), rotation: 45.0, shape: Rect),
                    (id: 2, position: (0.0, 0.0), shape: Polygon([(0.0, 0.0), (4.0, 0.0), (0.0, 4.5)])),
                    (id: 3, position: (9.0, 9.0), shape: Polyline([(1.0, 1.0), (2.0, 3.0)])),
                    (id: 4, position: (0.0, 0.0), shape: Tile(1)),
                    (id: 5, position: (0.0, 0.0), shape: Ellipse)]))]"##,
    );
    let first = TileMapAsset::from_ron(&text).expect("parses");
    let again = TileMapAsset::from_ron(&first.to_ron().expect("serializes")).expect("re-parses");
    assert_eq!(first, again);
    assert_eq!(first.properties["i"], PropertyValue::Int(-9_007_199_254_740_993));
    assert_eq!(first.properties["f"], PropertyValue::Float(0.1));
    assert_eq!(
        first.properties["s"],
        PropertyValue::String("hi \"there\"".into())
    );
    assert_eq!(first.background.as_deref(), Some("#80ff00ff"));
    let MapLayer::Tiles(t) = &first.layers[0] else {
        panic!("tiles")
    };
    assert_eq!((t.offset, t.opacity, t.visible), ([3.5, -2.25], 0.375, false));
    let MapLayer::Objects(o) = &first.layers[1] else {
        panic!("objects")
    };
    assert_eq!(
        o.objects[1].shape,
        ObjectShape::Polygon(vec![[0.0, 0.0], [4.0, 0.0], [0.0, 4.5]])
    );
    assert_eq!(o.objects[3].shape, ObjectShape::Tile(1));
    assert_eq!(o.objects[0].rotation, 45.0);
}

fn invalid(text: &str) -> String {
    match TileMapAsset::from_ron(text) {
        Err(TileMapError::Invalid(why)) => why,
        other => panic!("expected an Invalid error, got {other:?}"),
    }
}

/// Catches a size limit that is off by one or missing, and checks the cheap rejections happen
/// before any allocation sized by the claimed cell count.
#[test]
fn size_limits_reject_at_the_boundary_without_allocating() {
    // 8193 x 8192 cells is one row over the 64 Mi cell cap: rejected before the (empty) layer
    // list is looked at, so nothing near 256 MB is built.
    let why = invalid(&ron(
        "width: 8193, height: 8192, tile_width: 1, tile_height: 1, tilesets: [], layers: []",
    ));
    assert!(why.contains("too large"), "{why}");
    // Pixel size past 32 bits: 65536 * 65536.
    let why = invalid(&ron(
        "width: 65536, height: 1, tile_width: 65536, tile_height: 1, tilesets: [], layers: []",
    ));
    assert!(why.contains("overflows"), "{why}");
    for zero in [
        "width: 0, height: 1, tile_width: 8, tile_height: 8",
        "width: 1, height: 0, tile_width: 8, tile_height: 8",
        "width: 1, height: 1, tile_width: 0, tile_height: 8",
        "width: 1, height: 1, tile_width: 8, tile_height: 0",
    ] {
        let why = invalid(&ron(&format!("{zero}, tilesets: [], layers: []")));
        assert!(why.contains("positive"), "{zero}: {why}");
    }
    let layers = vec![r#"Objects((name: "o", objects: []))"#; 257].join(", ");
    let why = invalid(&ron(&format!(
        "width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [{layers}]"
    )));
    assert!(why.contains("256"), "{why}");
    let layers = vec![r#"Objects((name: "o", objects: []))"#; 256].join(", ");
    assert!(
        TileMapAsset::from_ron(&ron(&format!(
            "width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [{layers}]"
        )))
        .is_ok(),
        "256 layers is the limit, not 255"
    );
}

/// Catches a tile definition or animation frame one past the tileset being accepted, and unsorted
/// definitions (which break the binary search).
#[test]
fn tile_definitions_must_lie_inside_the_tileset() {
    let set = |tiles: &str| {
        ron(&format!(
            r#"width: 1, height: 1, tile_width: 8, tile_height: 8,
            tilesets: [(name: "a", first_gid: 1, image: "a.png", image_width: 32, image_height: 8,
                        tile_width: 8, tile_height: 8, columns: 4, tile_count: 4, tiles: [{tiles}])],
            layers: [Tiles((name: "g", data: [0]))]"#
        ))
    };
    assert!(
        TileMapAsset::from_ron(&set("(id: 3)")).is_ok(),
        "the last tile may carry data"
    );
    assert!(invalid(&set("(id: 4)")).contains("beyond"));
    assert!(invalid(&set("(id: 2), (id: 1)")).contains("sorted"));
    assert!(
        invalid(&set("(id: 1), (id: 1)")).contains("sorted"),
        "duplicates are not sorted either"
    );
    let anim = |frame: &str| set(&format!("(id: 0, animation: [{frame}])"));
    assert!(TileMapAsset::from_ron(&anim("(tile: 3, duration_ms: 1)")).is_ok());
    assert!(invalid(&anim("(tile: 4, duration_ms: 1)")).contains("animation frame"));
    assert!(invalid(&anim("(tile: 0, duration_ms: 0)")).contains("animation frame"));
}

/// Catches layer data of the wrong length, and a gid no tileset holds (flags must be ignored when
/// checking membership).
#[test]
fn layer_data_must_match_the_grid_and_use_known_gids() {
    let layer = |data: &str| {
        ron(&format!(
            "width: 2, height: 2, tile_width: 8, tile_height: 8, {TWO_SETS},
             layers: [Tiles((name: \"g\", data: [{data}]))]"
        ))
    };
    assert!(TileMapAsset::from_ron(&layer(&format!("0, 1, 11, {}", 4 | FLIP_X_BIT))).is_ok());
    assert!(invalid(&layer("0, 1, 11")).contains("expected 4"));
    assert!(invalid(&layer("0, 1, 11, 4, 4")).contains("expected 4"));
    assert!(invalid(&layer("0, 1, 11, 5")).contains("GID 5"));
    assert!(invalid(&layer("0, 1, 11, 12")).contains("GID 12"));
}

/// Catches syntax errors or missing fields panicking or being mislabelled as another error class.
#[test]
fn malformed_ron_is_a_parse_error_never_a_panic() {
    for text in [
        "",
        "(",
        "garbage",
        "()",
        "(version: 1)",
        "(version: 1, width: -1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [])",
        "(version: 1, width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [Tiles(",
    ] {
        let err = TileMapAsset::from_ron(text).expect_err(text);
        assert!(matches!(err, TileMapError::Parse(_)), "{text:?} -> {err:?}");
    }
    let newer = ron("width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: []").replacen(
        &format!("version: {TILEMAP_FORMAT_VERSION}"),
        &format!("version: {}", TILEMAP_FORMAT_VERSION + 1),
        1,
    );
    assert!(matches!(
        TileMapAsset::from_ron(&newer),
        Err(TileMapError::Version { found, .. }) if found == TILEMAP_FORMAT_VERSION + 1
    ));
}

/// Catches collision lookups depending on the flip flags of the cell, tiles without a definition
/// counting as solid, and the grid orientation (bottom row first).
#[test]
fn collision_ignores_flip_flags_and_undefined_tiles() {
    use TileCollision::{Empty, OneWay, Solid};
    // gid 2 = local 1 (solid), gid 3 = local 2 (one way), gid 1 = local 0 (no definition).
    let m = map(
        2,
        2,
        8,
        8,
        &[3, 2 | FLIP_X_BIT, 1 | FLIP_Y_BIT | FLIP_DIAGONAL_BIT, 0],
    );
    // Top row [OneWay, Solid]; bottom row [Empty, Empty]; the grid lists the bottom row first.
    assert_eq!(m.collision_grid(0), vec![Empty, Empty, OneWay, Solid]);
    assert_eq!(m.collision_grid_all(), vec![Empty, Empty, OneWay, Solid]);
    // The two layers combine with Solid winning.
    let mut stacked = m.clone();
    let mut extra = m.layers[0].clone();
    if let MapLayer::Tiles(layer) = &mut extra {
        layer.data = vec![2, 3, 0, 0];
    }
    stacked.layers.push(extra);
    assert_eq!(stacked.collision_grid_all(), vec![Empty, Empty, Solid, Solid]);
}

fn cooked() -> CookedTileMap {
    CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map: map(2, 1, 8, 8, &[2, 10]),
        images: vec![
            RgbaImage {
                width: 32,
                height: 8,
                rgba8: vec![7; 32 * 8 * 4],
            },
            RgbaImage {
                width: 16,
                height: 8,
                rgba8: vec![9; 16 * 8 * 4],
            },
        ],
    }
}

/// Catches a cooked payload that silently decodes from truncated or damaged bytes into a map that
/// passes validation with different content, and any panic on hostile bytes.
#[test]
fn cooked_payloads_reject_truncation_and_never_panic_on_damage() {
    let good = cooked();
    let bytes = good.to_postcard().expect("encodes");
    assert_eq!(CookedTileMap::from_postcard(&bytes).expect("decodes"), good);
    // Every strict prefix is an error (the images are at the end, so a short read is detected).
    for len in 0..bytes.len() {
        assert!(
            CookedTileMap::from_postcard(&bytes[..len]).is_err(),
            "a {len}-byte prefix of {} decoded",
            bytes.len()
        );
    }
    // Damage every byte in turn: the result may be Ok or Err, but never a panic, and an Ok result
    // must itself be valid.
    for i in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[i] ^= 0xA5;
        if let Ok(map) = CookedTileMap::from_postcard(&damaged) {
            assert!(
                map.validate().is_ok(),
                "byte {i} produced an invalid accepted map"
            );
        }
    }
}

/// Catches the tileset image being allowed to be smaller than the tiles it must supply.
#[test]
fn cooked_tile_images_must_cover_their_tiles() {
    let mut c = cooked();
    assert!(c.validate().is_ok());
    c.images[0].width = 24;
    c.images[0].rgba8.truncate(24 * 8 * 4);
    let err = c.validate().expect_err("tile 3 would lie outside a 24 px image");
    assert!(
        matches!(&err, TileMapError::Invalid(m) if m.contains("outside")),
        "{err}"
    );
}
