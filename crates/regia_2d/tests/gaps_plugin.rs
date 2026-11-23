// SPDX-License-Identifier: MPL-2.0
//! The headless half of the 2D plugin (test-gap audit): `extract_2d_frame` driven through a real
//! `App` without a GPU or an asset server: draw order from `Transform` z, visibility, missing
//! assets, statistics, and the cull window derived from the window size and camera rotation.

use regia_2d::atlas::{AtlasSprite, RgbaImage, SpriteAtlas};
use regia_2d::tilemap::{COOKED_TILEMAP_VERSION, CookedTileMap, TILEMAP_FORMAT_VERSION, TileMapAsset};
use regia_2d::{Frame2dStats, Sprite, Sprite2dPlugin, SpriteAssets, SpritePass, TileMap, View2d};
use regia_app::prelude::*;
use regia_core::math::{UVec2, Vec3};
use regia_scene::Transform;

fn atlas() -> SpriteAtlas {
    let mut atlas = SpriteAtlas::single(
        "dot",
        RgbaImage {
            width: 4,
            height: 4,
            rgba8: vec![255; 64],
        },
    );
    atlas.sprites.push(AtlasSprite {
        name: "other".into(),
        page: 0,
        rect: [0, 0, 2, 2],
        source_size: [2, 2],
        trim_offset: [0, 0],
        pivot: [0.5, 0.5],
        nine_slice: None,
    });
    atlas.sprites.sort_by(|a, b| a.name.cmp(&b.name));
    atlas
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    app
}

fn tinted(name: &str, atlas: &str, tint: f32) -> Sprite {
    Sprite {
        color: [tint, 1.0, 1.0, 1.0],
        ..Sprite::named(atlas, name)
    }
}

fn at_z(z: f32) -> Transform {
    Transform::from_translation(Vec3::new(0.0, 0.0, z))
}

/// Catches extraction drawing hidden sprites or ones with a missing atlas or sprite name (or
/// counting them), ordering by spawn order instead of z, and miscounting texture batches.
#[test]
fn extraction_orders_by_z_skips_the_undrawable_and_counts_batches() {
    let mut app = app();
    {
        let mut assets = app.world_mut().resource_mut::<SpriteAssets>();
        assets.insert_atlas("a", atlas());
        assets.insert_atlas("b", atlas());
    }
    let world = app.world_mut();
    world.spawn((tinted("dot", "a", 0.5), at_z(2.0)));
    world.spawn((tinted("dot", "a", 0.25), at_z(1.0)));
    world.spawn((tinted("dot", "b", 0.75), at_z(3.0)));
    world.spawn((
        Sprite {
            visible: false,
            ..tinted("dot", "a", 0.9)
        },
        at_z(0.0),
    ));
    world.spawn((tinted("dot", "no_such_atlas", 0.9), at_z(0.0)));
    world.spawn((tinted("no_such_sprite", "a", 0.9), at_z(0.0)));
    world.spawn((Sprite::default(), at_z(0.0)));
    let pass = world.resource::<SpritePass>().clone();
    app.update();

    let state = pass.0.lock().expect("pass lock");
    let order: Vec<f32> = state.prepared.instances.iter().map(|q| q.color[0]).collect();
    assert_eq!(order, vec![0.25, 0.5, 0.75]);
    assert_eq!(
        state.stats,
        Frame2dStats {
            sprites: 3,
            tiles: 0,
            instances: 3,
            batches: 2,
        }
    );
    assert_eq!(
        state.prepared.batches[0].1,
        0..2,
        "two atlas-a sprites, then the atlas-b one"
    );
    drop(state);
    assert_eq!(pass.stats().sprites, 3);
}

/// Catches a frame carrying over quads from the previous extraction.
#[test]
fn each_extraction_replaces_the_previous_frame() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<SpriteAssets>()
        .insert_atlas("a", atlas());
    let e = app.world_mut().spawn((tinted("dot", "a", 0.5), at_z(0.0))).id();
    let pass = app.world().resource::<SpritePass>().clone();
    app.update();
    assert_eq!(pass.stats().instances, 1);
    app.world_mut().despawn(e);
    app.update();
    assert_eq!(pass.stats(), Frame2dStats::default());
    assert!(pass.0.lock().expect("pass lock").prepared.instances.is_empty());
}

/// A 20x20 map of 8 px cells, all showing tile 1 of a 2x2-tile image.
fn big_map() -> CookedTileMap {
    let data = vec!["2"; 400].join(", ");
    let map = TileMapAsset::from_ron(&format!(
        r#"(version: {TILEMAP_FORMAT_VERSION}, width: 20, height: 20, tile_width: 8, tile_height: 8,
        tilesets: [(name: "t", first_gid: 1, image: "t.png", image_width: 16, image_height: 16,
                    tile_width: 8, tile_height: 8, columns: 2, tile_count: 4)],
        layers: [Tiles((name: "g", data: [{data}]))])"#
    ))
    .expect("valid map");
    CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map,
        images: vec![RgbaImage {
            width: 16,
            height: 16,
            rgba8: vec![255; 16 * 16 * 4],
        }],
    }
}

fn tiles_drawn(view: View2d, window: Option<UVec2>, hidden: bool) -> u32 {
    let mut app = app();
    app.world_mut()
        .resource_mut::<SpriteAssets>()
        .insert_tilemap("level", big_map());
    app.insert_resource(view);
    app.world_mut().spawn((
        TileMap {
            visible: !hidden,
            ..TileMap::new("level")
        },
        Transform::default(),
    ));
    let pass = app.world().resource::<SpritePass>().clone();
    pass.0.lock().expect("pass lock").last_window = window;
    app.update();
    pass.stats().tiles
}

/// Catches the cull window being ignored (the whole map drawn every frame), mis-sized for the
/// camera, or applied before any window size is known.
#[test]
fn tiles_are_culled_to_the_window_the_overlay_last_drew() {
    // Without a known window everything is drawn.
    assert_eq!(tiles_drawn(View2d::default(), None, false), 400);
    // A 16x16 window centred on the origin sees x, y in -8..8; with the one-tile margin the
    // visited cells are x 0..2 and the two bottom rows: 2 x 2 tiles.
    assert_eq!(tiles_drawn(View2d::default(), Some(UVec2::new(16, 16)), false), 4);
    // A hidden map component draws nothing at all.
    assert_eq!(tiles_drawn(View2d::default(), Some(UVec2::new(16, 16)), true), 0);
}

/// Catches a rotated camera culling with the unrotated rectangle (tiles in the rotated corners
/// would pop out): the view grows to its circumscribed square, 22.6 units wide for 16x16.
#[test]
fn a_rotated_camera_culls_with_the_circumscribed_square() {
    let mut view = View2d::default();
    view.camera.rotation = 0.5;
    // The square spans +-11.31, plus the 8 px margin: cells 0..3 in x and the bottom 3 rows.
    assert_eq!(tiles_drawn(view, Some(UVec2::new(16, 16)), false), 9);
}

/// Catches the pixel-perfect mode culling by the window instead of the virtual screen: the map is
/// drawn into the 16x16 virtual size, so a huge window must not enlarge the cull window.
#[test]
fn pixel_perfect_culls_by_the_virtual_size() {
    let view = View2d {
        pixel_perfect: Some(regia_2d::PixelPerfect::new(16, 16)),
        ..View2d::default()
    };
    assert_eq!(tiles_drawn(view, Some(UVec2::new(1920, 1080)), false), 4);
}
