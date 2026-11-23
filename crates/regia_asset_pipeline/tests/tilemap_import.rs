// SPDX-License-Identifier: MPL-2.0
//! Tiled import, `.tilemap.ron` round trip, project cook of 2D sources, and the Tiled sample's
//! lavapipe golden (`docs/specs/22-tier2.md` § T2.10).

use std::path::{Path, PathBuf};

use regia_2d::batch::Frame2d;
use regia_2d::render::{DrawSettings, render_offscreen};
use regia_2d::sprite::LoadedTileMap;
use regia_2d::tilemap::{MapLayer, ObjectShape, PropertyValue, TileCollision, TileMapAsset, split_gid};
use regia_2d::{OrthoCamera, TileMap, decode_atlas, decode_tilemap};
use regia_asset_pipeline::tiled_import::{cook_tilemap, import_tiled, tiled_to_tilemap_ron};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_core::math::{Affine3A, Vec2};
use regia_render::RenderDevice;
use regia_test_utils::golden::{PixelTolerance, check_pixel_golden};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiled")
}

fn sample() -> TileMapAsset {
    import_tiled(&fixtures().join("sample.tmx"))
        .expect("sample.tmx imports")
        .map
}

#[test]
fn tmx_sample_imports_layers_flips_collision_animation_and_objects() {
    let imported = import_tiled(&fixtures().join("sample.tmx")).expect("imports");
    assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
    let map = imported.map;
    assert_eq!(
        (map.width, map.height, map.tile_width, map.tile_height),
        (24, 12, 8, 8)
    );
    assert_eq!(map.background.as_deref(), Some("#ff203040"));
    assert_eq!(map.properties.get("gravity"), Some(&PropertyValue::Float(400.0)));
    let names: Vec<&str> = map.layers.iter().map(MapLayer::name).collect();
    assert_eq!(names, vec!["background", "props/platforms", "spawns"]);

    let set = &map.tilesets[0];
    assert_eq!(
        (set.name.as_str(), set.image.as_str(), set.columns, set.tile_count),
        ("terrain", "tileset.png", 20, 400)
    );

    // Corners of the frame and the one-way plank.
    assert_eq!(map.tile_at(0, 0, 0).map(|t| t.local_id), Some(0));
    assert_eq!(map.tile_at(0, 23, 11).map(|t| t.local_id), Some(42));
    let plank = map.tile_at(1, 3, 7).expect("plank");
    assert_eq!((plank.local_id, plank.collision()), (6, TileCollision::OneWay));
    // Mirrored plank, vertical and diagonal flips.
    let mirrored = map.tile_at(1, 15, 4).expect("mirrored plank");
    assert!(mirrored.flags.flip_x && mirrored.local_id == 9);
    assert!(
        map.tile_at(1, 12, 8)
            .is_some_and(|t| t.flags.flip_y && !t.flags.flip_x)
    );
    assert!(map.tile_at(1, 13, 8).is_some_and(|t| t.flags.diagonal));
    // Animated solid tile.
    let animated = map.tile_at(1, 11, 8).expect("animated");
    let def = animated.def().expect("tile data");
    assert_eq!((def.collision, def.animation.len()), (TileCollision::Solid, 2));
    assert_eq!(def.animation[1].tile, 4);

    let MapLayer::Tiles(platforms) = &map.layers[1] else {
        panic!("platforms is a tile layer");
    };
    assert_eq!(
        platforms.properties.get("collides"),
        Some(&PropertyValue::Bool(true))
    );
    let MapLayer::Objects(spawns) = &map.layers[2] else {
        panic!("spawns is an object layer");
    };
    assert_eq!(spawns.objects.len(), 2);
    assert_eq!(
        (spawns.objects[0].name.as_str(), &spawns.objects[0].shape),
        ("player_start", &ObjectShape::Point)
    );
    assert_eq!(spawns.objects[1].size, [16.0, 16.0]);
    assert_eq!(
        spawns.objects[1].properties.get("next_level"),
        Some(&PropertyValue::String("level2".into()))
    );

    // Collision grid: bottom row first; the frame's bottom row is solid, the plank row one-way, the
    // background interior empty.
    let grid = map.collision_grid_all();
    assert_eq!(grid[0], TileCollision::Solid);
    assert_eq!(grid[(12 - 1 - 7) * 24 + 3], TileCollision::OneWay);
    assert_eq!(grid[(12 - 1 - 5) * 24 + 5], TileCollision::Empty);
}

#[test]
fn tmj_imports_the_macroquad_map() {
    let map = import_tiled(&fixtures().join("macroquad_map.tmj"))
        .expect("imports")
        .map;
    assert_eq!((map.width, map.height, map.tile_width), (40, 19, 8));
    assert_eq!(map.tilesets.len(), 1);
    assert_eq!((map.tilesets[0].first_gid, map.tilesets[0].columns), (1, 20));
    let MapLayer::Tiles(layer) = &map.layers[0] else {
        panic!("tile layer");
    };
    assert_eq!(layer.data.len(), 40 * 19);
    assert_eq!(split_gid(layer.data[0]).0, 4);
}

#[test]
fn tilemap_ron_round_trips_the_import() {
    let (ron, warnings) = tiled_to_tilemap_ron(&fixtures().join("sample.tmx")).expect("converts");
    assert!(warnings.is_empty());
    let parsed = TileMapAsset::from_ron(&ron).expect("parses back");
    assert_eq!(parsed, sample());
}

#[test]
fn malformed_maps_are_errors_not_panics() {
    let dir = scratch("bad-maps");
    std::fs::write(dir.join("broken.tmx"), "<map><layer").expect("write");
    assert!(import_tiled(&dir.join("broken.tmx")).is_err());
    std::fs::write(dir.join("broken.tmj"), "{\"width\": 2}").expect("write");
    assert!(import_tiled(&dir.join("broken.tmj")).is_err());
    let odd = std::fs::read_to_string(fixtures().join("macroquad_map.tmj"))
        .expect("read")
        .replace("\"orthogonal\"", "\"spherical\"");
    std::fs::write(dir.join("odd.tmj"), odd).expect("write");
    let err = import_tiled(&dir.join("odd.tmj")).expect_err("an unknown orientation is unsupported");
    assert!(err.to_string().contains("orientation"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-2d-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn cook_project_cooks_tiled_maps_tilemap_ron_and_atlases() {
    let project = scratch("cook");
    let assets = project.join("assets/maps");
    std::fs::create_dir_all(&assets).expect("assets");
    for f in ["sample.tmx", "terrain.tsx", "tileset.png", "macroquad_map.tmj"] {
        std::fs::copy(fixtures().join(f), assets.join(f)).expect("copy fixture");
    }
    let (ron, _) = tiled_to_tilemap_ron(&fixtures().join("sample.tmx")).expect("converts");
    std::fs::write(assets.join("sample_copy.tilemap.ron"), ron).expect("write ron");
    std::fs::write(
        assets.join("ui.atlas.ron"),
        r#"(version: 1, sprites: [(name: "tiles", image: "tileset.png", grid: Some((8, 8)), pivot: (0.5, 0.0))])"#,
    )
    .expect("write atlas");

    let report = cook_project(CookOptions::for_host(project.clone())).expect("cook runs");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = regia_asset::cooked_cache_dir(&project, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("manifest exists");
    let read = |path: &str| {
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("{path} cooked"));
        std::fs::read(cache.join(regia_asset::cooked_file_name(entry.id))).expect("cooked file")
    };
    let tmx = decode_tilemap(&read("assets/maps/sample.tmx")).expect("cooked tmx");
    let ron = decode_tilemap(&read("assets/maps/sample_copy.tilemap.ron")).expect("cooked ron");
    assert_eq!(tmx, ron);
    assert_eq!((tmx.images[0].width, tmx.images[0].height), (160, 160));
    assert!(decode_tilemap(&read("assets/maps/macroquad_map.tmj")).is_ok());
    let atlas = decode_atlas(&read("assets/maps/ui.atlas.ron")).expect("cooked atlas");
    assert_eq!(atlas.sprites.len(), 400);
    assert!(atlas.sprite("tiles_120").is_some());
    // The tileset is read through the map, not cooked on its own.
    assert!(!manifest.entries.iter().any(|e| e.path.ends_with(".tsx")));

    // Incremental: nothing changed → everything skipped; editing the tileset image recooks the
    // maps and the atlas that read it.
    let again = cook_project(CookOptions::for_host(project.clone())).expect("recook");
    assert_eq!(again.cooked, 0, "{again:?}");
    let png = std::fs::read(assets.join("tileset.png")).expect("png");
    let mut img = image::load_from_memory(&png).expect("decode").to_rgba8();
    img.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
    img.save(assets.join("tileset.png")).expect("save");
    let edited = cook_project(CookOptions::for_host(project.clone())).expect("recook");
    assert!(edited.cooked >= 4, "{edited:?}");
    let _ = std::fs::remove_dir_all(&project);
}

/// The sample map rendered 1:1 on the CPU: what the GPU pass must produce exactly (nearest
/// sampling, integer placement, opaque or fully transparent texels).
fn cpu_reference(map: &TileMapAsset, tileset: &image::RgbaImage, background: [u8; 4]) -> Vec<u8> {
    let (w, h) = (map.width * map.tile_width, map.height * map.tile_height);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for px in out.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&background);
    }
    for layer in 0..map.layers.len() {
        for ty in 0..map.height {
            for tx in 0..map.width {
                let Some(tile) = map.tile_at(layer, tx, ty) else {
                    continue;
                };
                let Some([sx, sy, _, _]) = tile.set.tile_rect(tile.local_id) else {
                    continue;
                };
                for v in 0..map.tile_height {
                    for u in 0..map.tile_width {
                        // Same order as the shader: mirrors, then the transpose.
                        let (mut a, mut b) = (u, v);
                        if tile.flags.flip_y {
                            b = map.tile_height - 1 - b;
                        }
                        if tile.flags.flip_x {
                            a = map.tile_width - 1 - a;
                        }
                        if tile.flags.diagonal {
                            std::mem::swap(&mut a, &mut b);
                        }
                        let texel = tileset.get_pixel(sx + a, sy + b).0;
                        if texel[3] == 0 {
                            continue;
                        }
                        assert_eq!(texel[3], 255, "the reference assumes binary alpha");
                        let (x, y) = (tx * map.tile_width + u, ty * map.tile_height + v);
                        let at = ((y * w + x) * 4) as usize;
                        out[at..at + 4].copy_from_slice(&texel);
                    }
                }
            }
        }
    }
    out
}

#[test]
fn tiled_sample_renders_identically_to_its_lavapipe_golden() {
    let device = RenderDevice::headless().expect("headless GPU (lavapipe)");
    let map = sample();
    let cooked = cook_tilemap(map.clone(), &fixtures()).expect("cooks");
    let loaded = LoadedTileMap::new(cooked);
    let [w, h] = map.pixel_size();

    let mut frame = Frame2d::new();
    let tiles = frame.push_tilemap(&loaded, &TileMap::new("sample"), &Affine3A::IDENTITY, [1.0; 4]);
    assert!(tiles > 24 * 12);
    let settings = DrawSettings {
        camera: OrthoCamera {
            center: Vec2::new(w as f32 / 2.0, h as f32 / 2.0),
            pixels_per_unit: 1.0,
            rotation: 0.0,
        },
        pixel_perfect: None,
        snap: true,
        clear: None,
    };
    let background = [0x20, 0x30, 0x40, 0xff];
    let clear = background.map(|c| f64::from(c) / 255.0);
    let pixels = render_offscreen(
        &device,
        frame.prepare(),
        settings,
        w,
        h,
        wgpu::TextureFormat::Rgba8Unorm,
        clear,
    )
    .expect("renders");

    // Exact against the CPU composite: every tile, flip and layer in place.
    let tileset = image::open(fixtures().join("tileset.png"))
        .expect("tileset")
        .to_rgba8();
    let reference = cpu_reference(&map, &tileset, background);
    let wrong = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(reference.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    if wrong > 0 {
        let dump = std::env::temp_dir().join(format!("tiled_sample_actual-{}.png", std::process::id()));
        let _ = image::save_buffer(&dump, &pixels, w, h, image::ColorType::Rgba8);
        panic!(
            "{wrong} pixels differ from the CPU reference (actual written to {})",
            dump.display()
        );
    }

    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens/tiled_sample.png");
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        std::fs::create_dir_all(golden.parent().unwrap_or(Path::new("."))).expect("goldens dir");
        image::save_buffer(&golden, &pixels, w, h, image::ColorType::Rgba8).expect("bless");
        return;
    }
    let blessed = image::open(&golden)
        .unwrap_or_else(|e| {
            panic!(
                "golden {}: {e} (bless with REGIA_BLESS=1 on lavapipe)",
                golden.display()
            )
        })
        .to_rgba8()
        .into_raw();
    let exact = PixelTolerance {
        max_channel_diff: 0,
        outlier_ppm: 0,
    };
    match check_pixel_golden(
        &device.adapter.get_info(),
        "tiled_sample",
        &pixels,
        &blessed,
        w,
        h,
        exact,
    ) {
        Ok(verdict) => eprintln!("{verdict}"),
        Err(verdict) => panic!("Tiled sample drifted from its lavapipe golden: {verdict}"),
    }
}

/// The 2D plugin loads a cooked map through the asset server, and reloads it when the server's
/// hot-reload generation for its id moves (the hook the live cooker drives, specs/12 § 8).
#[test]
fn sprite2d_plugin_loads_cooked_maps_and_reloads_them_on_a_generation_bump() {
    use regia_app::prelude::*;
    use regia_asset::{AssetServer, AssetServerSettings};

    let project = scratch("reload");
    let assets = project.join("assets/maps");
    std::fs::create_dir_all(&assets).expect("assets");
    for f in ["sample.tmx", "terrain.tsx", "tileset.png"] {
        std::fs::copy(fixtures().join(f), assets.join(f)).expect("copy fixture");
    }
    let report = cook_project(CookOptions::for_host(project.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let server = AssetServer::open(AssetServerSettings {
        project_root: project.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, regia_2d::Sprite2dPlugin));
    app.insert_resource(server);
    app.world_mut().spawn(TileMap::new("assets/maps/sample.tmx"));
    app.update();
    let first = |app: &App| {
        app.world()
            .resource::<regia_2d::SpriteAssets>()
            .tilemap("assets/maps/sample.tmx")
            .and_then(|m| m.map.tile_at(0, 0, 0).map(|t| t.local_id))
    };
    assert_eq!(first(&app), Some(0), "loaded through the asset server");
    assert!(app.world().resource::<regia_2d::SpritePass>().stats().tiles > 0);

    // Edit the map (top-left tile 0 → 2), recook, and bump the generation as the watcher would.
    let tmx = std::fs::read_to_string(assets.join("sample.tmx")).expect("read");
    std::fs::write(assets.join("sample.tmx"), tmx.replacen("\n1,2,2,", "\n3,2,2,", 1)).expect("write");
    let again = cook_project(CookOptions::for_host(project.clone())).expect("recook");
    assert_eq!(again.cooked, 1, "{again:?}");
    app.update();
    assert_eq!(first(&app), Some(0), "no reload before the generation moves");
    {
        let mut server = app.world_mut().resource_mut::<AssetServer>();
        let id = server.id_for_path("assets/maps/sample.tmx").expect("id");
        server.reload_id(id).expect("reload");
    }
    app.update();
    assert_eq!(first(&app), Some(2), "reloaded in place");
    let _ = std::fs::remove_dir_all(&project);
}
