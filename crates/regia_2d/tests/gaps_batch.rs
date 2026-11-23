// SPDX-License-Identifier: MPL-2.0
//! Sprite and tile batching edge cases (test-gap audit): sort stability and key corner cases,
//! batch splits on texture changes, layer z stepping, tilemap transforms, GID flag handling,
//! culling margins, 9-slice mirroring and size/anchor overrides. CPU only (no GPU device).

use std::ops::Range;

use regia_2d::atlas::{AtlasSprite, NineSlice, RgbaImage, SpriteAtlas};
use regia_2d::batch::{FLAG_DIAGONAL, FLAG_FLIP_X, FLAG_FLIP_Y, Frame2d, SpriteInstance, animated_tile};
use regia_2d::sprite::{LoadedAtlas, LoadedTileMap};
use regia_2d::tilemap::{
    COOKED_TILEMAP_VERSION, CookedTileMap, FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, ROTATE_HEX_BIT,
    TileMapAsset,
};
use regia_2d::{Rect2, Sprite, TileMap};
use regia_core::math::{Affine3A, Vec3};

fn image(w: u32, h: u32) -> RgbaImage {
    RgbaImage {
        width: w,
        height: h,
        rgba8: vec![255; (w * h * 4) as usize],
    }
}

fn sprite_entry(
    name: &str,
    rect: [u32; 4],
    source: [u32; 2],
    trim: [u32; 2],
    pivot: [f32; 2],
) -> AtlasSprite {
    AtlasSprite {
        name: name.into(),
        page: 0,
        rect,
        source_size: source,
        trim_offset: trim,
        pivot,
        nine_slice: None,
    }
}

/// A 64x32 page with a plain, a trimmed and a 9-slice (left 2, right 6, top 3, bottom 1) sprite.
fn atlas() -> LoadedAtlas {
    let mut atlas = SpriteAtlas::single("full", image(64, 32));
    atlas.sprites.push(sprite_entry(
        "trimmed",
        [16, 8, 4, 2],
        [10, 10],
        [3, 1],
        [0.5, 0.0],
    ));
    atlas.sprites.push(sprite_entry(
        "lopsided",
        [32, 0, 4, 2],
        [10, 10],
        [1, 0],
        [0.0, 0.0],
    ));
    let mut panel = sprite_entry("panel", [0, 0, 12, 12], [12, 12], [0, 0], [0.0, 0.0]);
    panel.nine_slice = Some(NineSlice {
        left: 2,
        top: 3,
        right: 6,
        bottom: 1,
    });
    atlas.sprites.push(panel);
    atlas.sprites.sort_by(|a, b| a.name.cmp(&b.name));
    LoadedAtlas::new(atlas)
}

fn at(x: f32, y: f32, z: f32) -> Affine3A {
    Affine3A::from_translation(Vec3::new(x, y, z))
}

fn tints(instances: &[SpriteInstance]) -> Vec<f32> {
    instances.iter().map(|q| q.color[0]).collect()
}

/// Catches the instance layout drifting from what the vertex shader reads (60 bytes, no padding).
#[test]
fn the_instance_is_sixty_bytes() {
    assert_eq!(std::mem::size_of::<SpriteInstance>(), 60);
}

/// Catches a sort that is not stable (equal z must keep submission order) or batches that merge
/// non-adjacent runs of one texture, which would reorder the painter's algorithm.
#[test]
fn equal_z_keeps_submission_order_and_alternating_textures_do_not_merge() {
    let a = atlas();
    let b = atlas();
    let s = Sprite::rect("x", [0, 0, 1, 1]);
    let mut frame = Frame2d::new();
    for (i, atlas) in [&a, &b, &a, &b, &a].into_iter().enumerate() {
        assert!(frame.push_sprite(atlas, &s, &at(0.0, 0.0, 1.0), [i as f32 / 10.0, 1.0, 1.0, 1.0]));
    }
    let prepared = frame.prepare();
    assert_eq!(tints(&prepared.instances), vec![0.0, 0.1, 0.2, 0.3, 0.4]);
    let ranges: Vec<Range<u32>> = prepared.batches.iter().map(|(_, r)| r.clone()).collect();
    assert_eq!(ranges, vec![0..1, 1..2, 2..3, 3..4, 4..5]);
    assert_eq!(prepared.batches[0].0.key, prepared.batches[2].0.key);
    assert_ne!(prepared.batches[0].0.key, prepared.batches[1].0.key);
}

/// Catches negative z sorting in front of positive z, and a fractional z being rounded.
#[test]
fn negative_and_fractional_z_sort_numerically() {
    let a = atlas();
    let s = Sprite::rect("x", [0, 0, 1, 1]);
    let mut frame = Frame2d::new();
    for (tint, z) in [
        (0.3, 0.5),
        (0.1, -2.0),
        (0.4, 1.0e6),
        (0.2, -0.25),
        (0.35, 0.5000001),
    ] {
        frame.push_sprite(&a, &s, &at(0.0, 0.0, z), [tint, 1.0, 1.0, 1.0]);
    }
    assert_eq!(tints(&frame.prepare().instances), vec![0.1, 0.2, 0.3, 0.35, 0.4]);
}

/// Catches `-0.0` and `0.0` being ordered apart: they are equal depths, so submission order must
/// decide (a sprite whose z came out of a negation would otherwise jump behind its neighbours).
#[test]
fn negative_zero_z_is_equal_to_zero_and_keeps_submission_order() {
    let a = atlas();
    let s = Sprite::rect("x", [0, 0, 1, 1]);
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &s, &at(0.0, 0.0, 0.0), [0.1, 1.0, 1.0, 1.0]);
    frame.push_sprite(&a, &s, &at(0.0, 0.0, -0.0), [0.2, 1.0, 1.0, 1.0]);
    frame.push_sprite(&a, &s, &at(0.0, 0.0, 0.0), [0.3, 1.0, 1.0, 1.0]);
    assert_eq!(tints(&frame.prepare().instances), vec![0.1, 0.2, 0.3]);
}

/// Catches a 9-slice sprite without an explicit size being split anyway (it has no stretch target,
/// so it must draw as one source-sized quad).
#[test]
fn a_nine_slice_sprite_without_a_size_is_one_quad() {
    let a = atlas();
    let mut frame = Frame2d::new();
    assert!(frame.push_sprite(&a, &Sprite::named("a", "panel"), &Affine3A::IDENTITY, [1.0; 4]));
    assert_eq!(frame.len(), 1);
    let q = frame.prepare().instances[0];
    assert_eq!((q.axis_x, q.axis_y), ([12.0, 0.0], [0.0, 12.0]));
    assert_eq!(q.uv_min, [0.0, 0.0]);
    assert_eq!(q.uv_max, [12.0 / 64.0, 12.0 / 32.0]);
}

/// Catches a mirrored 9-slice keeping its borders on the unmirrored side: with flip_x the left
/// border (2 px) must end up on the right, the right border (6 px) on the left.
#[test]
fn a_flipped_nine_slice_mirrors_its_asymmetric_borders() {
    let a = atlas();
    let mut sprite = Sprite::named("a", "panel");
    sprite.size = Some([40.0, 20.0]);
    sprite.anchor = Some([0.0, 0.0]);
    sprite.flip_x = true;
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &sprite, &Affine3A::IDENTITY, [1.0; 4]);
    let quads = frame.prepare().instances;
    assert_eq!(quads.len(), 9);
    assert!(quads.iter().all(|q| q.flags == FLAG_FLIP_X));
    // Source left column starts at u = 0 and is 2 px wide: it sits at the right edge (x 38..40).
    let left_col: Vec<_> = quads.iter().filter(|q| q.uv_min[0] == 0.0).collect();
    assert_eq!(left_col.len(), 3);
    assert!(
        left_col
            .iter()
            .all(|q| q.origin[0] == 38.0 && q.axis_x == [2.0, 0.0])
    );
    // Source right column is the last 6 px (u = 6/64 .. 12/64): it sits at the left edge (x 0..6).
    let right_col: Vec<_> = quads.iter().filter(|q| q.uv_min[0] == 6.0 / 64.0).collect();
    assert_eq!(right_col.len(), 3);
    assert!(
        right_col
            .iter()
            .all(|q| q.origin[0] == 0.0 && q.axis_x == [6.0, 0.0])
    );
    // Rows: bottom border 1 px, top border 3 px, middle 16 px stretched.
    let mut heights: Vec<f32> = quads.iter().map(|q| q.axis_y[1]).collect();
    heights.sort_by(f32::total_cmp);
    assert_eq!(heights, vec![1.0, 1.0, 1.0, 3.0, 3.0, 3.0, 16.0, 16.0, 16.0]);
}

/// Catches a flipped-Y 9-slice leaving the top border at the top.
#[test]
fn a_flipped_y_nine_slice_swaps_top_and_bottom_borders() {
    let a = atlas();
    let mut sprite = Sprite::named("a", "panel");
    sprite.size = Some([40.0, 20.0]);
    sprite.anchor = Some([0.0, 0.0]);
    sprite.flip_y = true;
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &sprite, &Affine3A::IDENTITY, [1.0; 4]);
    let quads = frame.prepare().instances;
    assert!(quads.iter().all(|q| q.flags == FLAG_FLIP_Y));
    // Source top row (v from 0, 3 px tall) goes to the bottom: y 0..3.
    let top_row: Vec<_> = quads.iter().filter(|q| q.uv_min[1] == 0.0).collect();
    assert_eq!(top_row.len(), 3);
    assert!(
        top_row
            .iter()
            .all(|q| q.origin[1] == 0.0 && q.axis_y == [0.0, 3.0])
    );
    // Source bottom row (last 1 px) goes to the top: y 19..20.
    let bottom_row: Vec<_> = quads.iter().filter(|q| q.uv_min[1] == 11.0 / 32.0).collect();
    assert_eq!(bottom_row.len(), 3);
    assert!(
        bottom_row
            .iter()
            .all(|q| q.origin[1] == 19.0 && q.axis_y == [0.0, 1.0])
    );
}

/// Catches a size override that is not applied to trim and pivot: the trimmed rectangle scales
/// with the drawn size and the pivot is taken in the drawn size.
#[test]
fn a_size_override_scales_the_trim_and_the_pivot() {
    let a = atlas();
    let mut sprite = Sprite::named("a", "trimmed");
    sprite.size = Some([20.0, 20.0]);
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &sprite, &Affine3A::IDENTITY, [1.0; 4]);
    let q = frame.prepare().instances[0];
    // Pivot (0.5, 0) of 20x20 -> base (-10, 0); scale 2; trim (3, 1), 4x2 inside 10x10:
    // x = -10 + 3*2, y = (10 - (1 + 2)) * 2 = 14.
    assert_eq!(q.origin, [-4.0, 14.0]);
    assert_eq!((q.axis_x, q.axis_y), ([8.0, 0.0], [0.0, 4.0]));
}

/// Catches the anchor override being ignored for rectangle sources (animated sheets), which would
/// always pivot on the centre.
#[test]
fn a_rect_sprite_pivots_on_the_anchor_or_the_centre() {
    let a = atlas();
    let mut frame = Frame2d::new();
    let mut corner = Sprite::rect("x", [0, 0, 8, 4]);
    corner.anchor = Some([1.0, 1.0]);
    frame.push_sprite(&a, &corner, &at(100.0, 50.0, 0.0), [0.1; 4]);
    frame.push_sprite(
        &a,
        &Sprite::rect("x", [0, 0, 8, 4]),
        &at(100.0, 50.0, 0.0),
        [0.2; 4],
    );
    let q = frame.prepare().instances;
    assert_eq!(q[0].origin, [92.0, 46.0]);
    assert_eq!(q[1].origin, [96.0, 48.0]);
}

/// Catches the mirror of a trimmed quad being taken from the wrong side: with the trim only on the
/// left (1 px) a horizontal flip moves the quad to 10 - (1 + 4) = 5 inside the box; Y is untouched.
#[test]
fn flip_x_mirrors_an_asymmetric_trim_inside_the_source_box() {
    let a = atlas();
    let plain = Sprite::named("a", "lopsided");
    let mut flipped = plain.clone();
    flipped.flip_x = true;
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &plain, &Affine3A::IDENTITY, [0.1; 4]);
    frame.push_sprite(&a, &flipped, &Affine3A::IDENTITY, [0.2; 4]);
    let q = frame.prepare().instances;
    // Pivot (0, 0): the box starts at the origin; y = 10 - (0 + 2) = 8 for both.
    assert_eq!(q[0].origin, [1.0, 8.0]);
    assert_eq!(q[1].origin, [5.0, 8.0]);
    assert_eq!((q[0].flags, q[1].flags), (0, FLAG_FLIP_X));
}

/// A map of `w`x`h` 8 px cells; tileset `t` (gids 1..=4, 16x16 image, 2 columns) with tile 0
/// animated as tile 0 for 100 ms, tile 3 for 0 ms, tile 2 for 50 ms.
fn ron_map(w: u32, h: u32, layers: &str, extra_sets: &str) -> TileMapAsset {
    let text = format!(
        r#"(version: {v}, width: {w}, height: {h}, tile_width: 8, tile_height: 8,
            tilesets: [
              (name: "t", first_gid: 1, image: "t.png", image_width: 16, image_height: 16,
               tile_width: 8, tile_height: 8, columns: 2, tile_count: 4,
               tiles: [(id: 0, animation: [(tile: 0, duration_ms: 100), (tile: 3, duration_ms: 1), (tile: 2, duration_ms: 50)])]){extra_sets}
            ],
            layers: [{layers}])"#,
        v = regia_2d::TILEMAP_FORMAT_VERSION
    );
    TileMapAsset::from_ron(&text).expect("test map parses")
}

fn loaded(map: TileMapAsset) -> LoadedTileMap {
    let images = map.tilesets.iter().map(|_| image(16, 16)).collect();
    LoadedTileMap::new(CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map,
        images,
    })
}

fn data_row(w: u32, h: u32, gid: u32) -> String {
    vec![gid.to_string(); (w * h) as usize].join(", ")
}

/// Catches uneven animation frames being mis-timed, a zero-length frame ever being shown, and a
/// huge clock value overflowing the cycle arithmetic.
#[test]
fn an_animated_tile_steps_through_uneven_frames_and_survives_huge_clocks() {
    let mut map = ron_map(1, 1, "Tiles((name: \"g\", data: [2]))", "");
    {
        let set = &map.tilesets[0];
        // Cycle 151 ms: 0..100 tile 0, 100 tile 3, 101..151 tile 2.
        assert_eq!(animated_tile(set, 0, 0), 0);
        assert_eq!(animated_tile(set, 0, 99), 0);
        assert_eq!(animated_tile(set, 0, 100), 3);
        assert_eq!(animated_tile(set, 0, 101), 2);
        assert_eq!(animated_tile(set, 0, 150), 2);
        assert_eq!(animated_tile(set, 0, 151), 0, "wraps");
        assert_eq!(
            animated_tile(set, 0, u64::MAX),
            animated_tile(set, 0, u64::MAX % 151)
        );
        // A tile without animation data shows itself.
        assert_eq!(animated_tile(set, 1, 12345), 1);
        assert_eq!(animated_tile(set, 99, 5), 99);
    }
    // A validated map has no zero durations, but the pass must not depend on it: the zero-length
    // middle frame is never shown, 100..150 is tile 2.
    map.tilesets[0].tiles[0].animation[1].duration_ms = 0;
    let set = &map.tilesets[0];
    assert_eq!(animated_tile(set, 0, 99), 0);
    assert_eq!(animated_tile(set, 0, 100), 2);
    assert_eq!(animated_tile(set, 0, 149), 2);
    assert_eq!(animated_tile(set, 0, 150), 0);
}

/// Catches layer z being counted over tile layers only, instead of the layer index in the map:
/// the third layer (index 2) sits at 2 steps even though only two layers have tiles, and a sprite
/// between steps sorts between them.
#[test]
fn layer_z_uses_the_layer_index_and_interleaves_with_sprites() {
    let layers = format!(
        "Tiles((name: \"a\", data: [{g}])), Objects((name: \"o\", objects: [])), Tiles((name: \"b\", data: [{g}]))",
        g = 2
    );
    let l = loaded(ron_map(1, 1, &layers, ""));
    let a = atlas();
    let component = TileMap {
        layer_z_step: 1.0,
        ..TileMap::new("m")
    };
    let mut frame = Frame2d::new();
    // Submitted last, but z = 1.5 puts it between layer 0 (z 0) and the third layer (z 2).
    assert_eq!(
        frame.push_tilemap(&l, &component, &Affine3A::IDENTITY, [0.5; 4]),
        2
    );
    frame.push_sprite(&a, &Sprite::rect("x", [0, 0, 1, 1]), &at(0.0, 0.0, 1.5), [0.9; 4]);
    let q = frame.prepare().instances;
    assert_eq!(tints(&q), vec![0.5, 0.9, 0.5]);
    // And a sprite at z 1.0 (the empty object layer's step) is still behind the third layer.
    let mut frame = Frame2d::new();
    frame.push_sprite(&a, &Sprite::rect("x", [0, 0, 1, 1]), &at(0.0, 0.0, 1.9), [0.9; 4]);
    frame.push_tilemap(&l, &component, &Affine3A::IDENTITY, [0.5; 4]);
    let q = frame.prepare().instances;
    assert_eq!(tints(&q), vec![0.5, 0.9, 0.5]);
}

/// Catches the map transform being applied to positions but not to the edge vectors (scaled maps
/// would draw unscaled tiles), or the bottom-up row order being lost under a translation.
#[test]
fn a_scaled_translated_map_places_tiles_bottom_up_in_world_space() {
    let l = loaded(ron_map(2, 2, "Tiles((name: \"g\", data: [2, 0, 0, 3]))", ""));
    let transform = Affine3A::from_scale_rotation_translation(
        Vec3::new(2.0, 3.0, 1.0),
        regia_core::math::Quat::IDENTITY,
        Vec3::new(100.0, 200.0, 5.0),
    );
    let mut frame = Frame2d::new();
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &transform, [1.0; 4]),
        2
    );
    let q = frame.prepare().instances;
    // Cell (0, 0) is the top-left: local bottom-left (0, 8) -> world (100, 200 + 24).
    // Cell (1, 1) is the bottom-right: local (8, 0) -> world (116, 200).
    assert_eq!(q[0].origin, [100.0, 224.0]);
    assert_eq!((q[0].axis_x, q[0].axis_y), ([16.0, 0.0], [0.0, 24.0]));
    assert_eq!(q[1].origin, [116.0, 200.0]);
    // gid 2 = local tile 1 -> source [8, 0, 8, 8]; gid 3 = local tile 2 -> [0, 8, 8, 8].
    assert_eq!((q[0].uv_min, q[0].uv_max), ([0.5, 0.0], [1.0, 0.5]));
    assert_eq!((q[1].uv_min, q[1].uv_max), ([0.0, 0.5], [0.5, 1.0]));
}

/// Catches GID flag decoding in the pass: the three Tiled flips map to the three instance bits, the
/// hexagonal rotation bit is masked off, and a flag-only value (gid 0) draws nothing.
#[test]
fn gid_flags_become_instance_bits_and_flag_only_cells_are_empty() {
    let cells = [
        2 | FLIP_DIAGONAL_BIT | FLIP_Y_BIT,
        2 | FLIP_X_BIT | FLIP_Y_BIT | FLIP_DIAGONAL_BIT,
        2 | ROTATE_HEX_BIT,
        FLIP_X_BIT,
        0,
        9,
    ];
    let mut map = ron_map(
        6,
        1,
        &format!("Tiles((name: \"g\", data: [{}]))", data_row(6, 1, 0)),
        "",
    );
    if let Some(regia_2d::tilemap::MapLayer::Tiles(layer)) = map.layers.first_mut() {
        layer.data = cells.to_vec();
    }
    let l = loaded(map);
    let mut frame = Frame2d::new();
    // Gid 9 has no tileset, gid 0 and the flag-only cell are empty.
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
        3
    );
    let q = frame.prepare().instances;
    assert_eq!(q[0].flags, FLAG_DIAGONAL | FLAG_FLIP_Y);
    assert_eq!(q[1].flags, FLAG_DIAGONAL | FLAG_FLIP_Y | FLAG_FLIP_X);
    assert_eq!(q[2].flags, 0, "the hexagonal rotation bit is not a flip");
}

/// Catches an invisible layer, a zero opacity layer and an object layer producing quads, and layer
/// opacity not multiplying the tint's alpha.
#[test]
fn hidden_transparent_and_object_layers_draw_nothing_and_opacity_scales_alpha() {
    let layers = r#"
        Tiles((name: "hidden", visible: false, data: [2])),
        Tiles((name: "clear", opacity: 0.0, data: [2])),
        Objects((name: "o", objects: [])),
        Tiles((name: "half", opacity: 0.5, data: [2]))"#;
    let l = loaded(ron_map(1, 1, layers, ""));
    let mut frame = Frame2d::new();
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0, 0.5, 0.25, 0.5]),
        1
    );
    let q = frame.prepare().instances;
    assert_eq!(q[0].color, [1.0, 0.5, 0.25, 0.25]);
}

/// Catches two tilesets sharing one batch: tiles of different images must split the draw even
/// inside one layer, and each tile must sample its own tileset's rectangle.
#[test]
fn tiles_of_two_tilesets_split_into_batches_by_image() {
    let second = r#",
              (name: "u", first_gid: 5, image: "u.png", image_width: 16, image_height: 16,
               tile_width: 8, tile_height: 8, columns: 2, tile_count: 4)"#;
    let l = loaded(ron_map(4, 1, "Tiles((name: \"g\", data: [2, 5, 2, 8]))", second));
    let mut frame = Frame2d::new();
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
        4
    );
    let p = frame.prepare();
    let ranges: Vec<Range<u32>> = p.batches.iter().map(|(_, r)| r.clone()).collect();
    assert_eq!(ranges, vec![0..1, 1..2, 2..3, 3..4]);
    assert_eq!(p.batches[0].0.key, p.batches[2].0.key);
    assert_ne!(p.batches[0].0.key, p.batches[1].0.key);
    assert_eq!(
        p.instances[1].uv_min,
        [0.0, 0.0],
        "gid 5 is tile 0 of the second set"
    );
    assert_eq!(
        p.instances[3].uv_min,
        [0.5, 0.5],
        "gid 8 is tile 3 of the second set"
    );
}

/// Catches the view-culling window dropping a tile that intersects the view, or not culling tiles
/// far outside it (the whole map would be drawn every frame).
#[test]
fn culling_keeps_every_tile_touching_the_view_and_drops_the_far_ones() {
    let l = loaded(ron_map(
        10,
        10,
        &format!("Tiles((name: \"g\", data: [{}]))", data_row(10, 10, 2)),
        "",
    ));
    let view = Rect2::new(35.0, 35.0, 10.0, 10.0);
    let mut frame = Frame2d::new();
    frame.visible = Some(view);
    let drawn = frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]);
    let q = frame.prepare().instances;
    assert_eq!(q.len(), drawn);
    assert!(drawn < 100, "culling did nothing: {drawn}");
    let has = |x: f32, y: f32| q.iter().any(|i| i.origin == [x, y]);
    // The view spans x 35..45, y 35..45: cells with origins 32, 40 and the one at 40.. overlap it.
    for x in [32.0, 40.0] {
        for y in [32.0, 40.0] {
            assert!(has(x, y), "tile at ({x}, {y}) touches the view but was culled");
        }
    }
    // Two tiles away in every direction is certainly outside.
    for far in [0.0, 8.0, 72.0] {
        assert!(!has(far, far), "tile at ({far}, {far}) is far outside the view");
    }
    // A view beyond the map draws nothing and never panics.
    let mut frame = Frame2d::new();
    frame.visible = Some(Rect2::new(-1000.0, -1000.0, 10.0, 10.0));
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
        0
    );
}

/// Catches a tileset whose tiles are larger than the grid (tall trees) being anchored on the cell's
/// top-left instead of its bottom-left, or being culled while their top is still on screen.
#[test]
fn oversized_tiles_anchor_on_the_cell_bottom_left_and_survive_culling() {
    let text = r#"(version: 1, width: 4, height: 4, tile_width: 8, tile_height: 8,
        tilesets: [(name: "tall", first_gid: 1, image: "t.png", image_width: 16, image_height: 32,
                    tile_width: 16, tile_height: 32, columns: 1, tile_count: 1)],
        layers: [Tiles((name: "g", data: [0,0,0,0, 0,0,0,0, 0,0,0,0, 1,0,0,0]))])"#
        .replace(
            "version: 1",
            &format!("version: {}", regia_2d::TILEMAP_FORMAT_VERSION),
        );
    let l = loaded(TileMapAsset::from_ron(&text).expect("parses"));
    // The tile sits in the bottom-left cell (origin 0, 0) and reaches up to y = 32.
    let mut frame = Frame2d::new();
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
        1
    );
    let q = frame.prepare().instances[0];
    assert_eq!(q.origin, [0.0, 0.0]);
    assert_eq!((q.axis_x, q.axis_y), ([16.0, 0.0], [0.0, 32.0]));
    // A view covering only the top of the 32 px tile (y 28..30, x 10..12) must still draw it.
    let mut frame = Frame2d::new();
    frame.visible = Some(Rect2::new(10.0, 28.0, 2.0, 2.0));
    assert_eq!(
        frame.push_tilemap(&l, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
        1
    );
}
