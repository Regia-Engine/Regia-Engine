// SPDX-License-Identifier: MPL-2.0
//! The sprite pass on a real device (lavapipe in CI): pixel-perfect camera goldens at three
//! integer scales, exact upscaling and letterbox bars, snapping, sprite pivots/flips and 9-slices,
//! and the ECS path (`Sprite2dPlugin` → extract → overlay).

use std::path::PathBuf;

use regia_2d::atlas::{AtlasSprite, NineSlice, RgbaImage, SpriteAtlas};
use regia_2d::batch::Frame2d;
use regia_2d::render::{DrawSettings, render_offscreen};
use regia_2d::sprite::LoadedAtlas;
use regia_2d::{OrthoCamera, PixelPerfect, Sprite};
use regia_core::math::{Affine3A, UVec2, Vec2, Vec3};
use regia_render::RenderDevice;
use regia_test_utils::golden::{PixelTolerance, check_pixel_golden};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const VIRTUAL: UVec2 = UVec2::new(64, 36);
const BAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const SKY: [f32; 4] = [0.2, 0.4, 0.8, 1.0];

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless GPU (lavapipe)")
}

/// One 32×16 page: an 8×8 four-colour "hero" with a marked corner, a 12×12 framed panel (9-slice
/// border 3) and a 4×4 white dot.
fn atlas() -> LoadedAtlas {
    let (w, h) = (32u32, 16u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut put = |x: u32, y: u32, c: [u8; 4]| {
        let at = ((y * w + x) * 4) as usize;
        rgba[at..at + 4].copy_from_slice(&c);
    };
    for y in 0..8 {
        for x in 0..8 {
            let c = match (x < 4, y < 4) {
                (true, true) => [255, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 255, 255],
                (false, false) => [255, 255, 0, 255],
            };
            put(x, y, c);
        }
    }
    put(0, 0, [255, 255, 255, 255]);
    for y in 0..12 {
        for x in 0..12 {
            let border = x < 3 || y < 3 || x >= 9 || y >= 9;
            put(
                8 + x,
                y,
                if border {
                    [120, 60, 20, 255]
                } else {
                    [240, 220, 180, 255]
                },
            );
        }
    }
    put(8, 0, [255, 0, 255, 255]);
    for y in 0..4 {
        for x in 0..4 {
            put(20 + x, y, [255, 255, 255, 255]);
        }
    }
    let sprite = |name: &str, rect: [u32; 4], pivot: [f32; 2], nine: Option<NineSlice>| AtlasSprite {
        name: name.into(),
        page: 0,
        rect,
        source_size: [rect[2], rect[3]],
        trim_offset: [0, 0],
        pivot,
        nine_slice: nine,
    };
    LoadedAtlas::new(SpriteAtlas {
        version: 1,
        pages: vec![RgbaImage {
            width: w,
            height: h,
            rgba8: rgba,
        }],
        sprites: vec![
            sprite("dot", [20, 0, 4, 4], [0.5, 0.5], None),
            sprite("hero", [0, 0, 8, 8], [0.0, 0.0], None),
            sprite(
                "panel",
                [8, 0, 12, 12],
                [0.0, 0.0],
                Some(NineSlice {
                    left: 3,
                    top: 3,
                    right: 3,
                    bottom: 3,
                }),
            ),
        ],
    })
}

/// The test scene in world units (= virtual pixels): a hero at (4, 4), a mirrored hero, a
/// stretched panel, and a dot on top.
fn scene(atlas: &LoadedAtlas) -> Frame2d {
    let mut frame = Frame2d::new();
    let at = |x: f32, y: f32, z: f32| Affine3A::from_translation(Vec3::new(x, y, z));
    assert!(frame.push_sprite(atlas, &Sprite::named("a", "hero"), &at(4.0, 4.0, 1.0), [1.0; 4]));
    let mut mirrored = Sprite::named("a", "hero");
    mirrored.flip_x = true;
    assert!(frame.push_sprite(atlas, &mirrored, &at(14.0, 4.0, 1.0), [1.0; 4]));
    let mut panel = Sprite::named("a", "panel");
    panel.size = Some([30.0, 14.0]);
    assert!(frame.push_sprite(atlas, &panel, &at(28.0, 16.0, 0.0), [1.0; 4]));
    assert!(frame.push_sprite(atlas, &Sprite::named("a", "dot"), &at(43.0, 23.0, 2.0), [1.0; 4]));
    frame
}

fn settings(center: Vec2) -> DrawSettings {
    DrawSettings {
        camera: OrthoCamera {
            center,
            pixels_per_unit: 1.0,
            rotation: 0.0,
        },
        pixel_perfect: Some(PixelPerfect {
            virtual_size: VIRTUAL,
            bar_color: BAR,
        }),
        snap: true,
        clear: Some(SKY),
    }
}

fn render(device: &RenderDevice, window: UVec2, center: Vec2) -> Vec<u8> {
    render_offscreen(
        device,
        scene(&atlas()).prepare(),
        settings(center),
        window.x,
        window.y,
        FORMAT,
        [1.0, 0.0, 1.0, 1.0],
    )
    .expect("renders")
}

fn px(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * width + x) * 4) as usize;
    [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("goldens/{name}.png"))
}

fn write_png(path: &PathBuf, width: u32, height: u32, pixels: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("goldens dir")).expect("create goldens dir");
    let file = std::fs::File::create(path).expect("create golden");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

fn read_png(path: &PathBuf) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| {
        panic!(
            "golden {}: {e} (bless with REGIA_BLESS=1 on lavapipe)",
            path.display()
        )
    });
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("png");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn check_golden(device: &RenderDevice, name: &str, window: UVec2, pixels: &[u8]) {
    let path = golden_path(name);
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        write_png(&path, window.x, window.y, pixels);
        return;
    }
    let (w, h, golden) = read_png(&path);
    assert_eq!((w, h), (window.x, window.y), "{name} golden size");
    let exact = PixelTolerance {
        max_channel_diff: 0,
        outlier_ppm: 0,
    };
    match check_pixel_golden(&device.adapter.get_info(), name, pixels, &golden, w, h, exact) {
        Ok(verdict) => eprintln!("{verdict}"),
        Err(verdict) => panic!("{name}: {verdict}"),
    }
}

#[test]
fn pixel_perfect_camera_matches_its_goldens_at_three_integer_scales() {
    let device = device();
    let center = Vec2::new(32.0, 18.0);
    let base = render(&device, VIRTUAL, center);
    // Scale 1 shows the virtual screen 1:1: the hero's marked texel (top-left of the sprite) sits
    // at virtual (4, 36 - 4 - 8) = (4, 24) from the top, the mirrored hero's at (21, 24).
    assert_eq!(px(&base, VIRTUAL.x, 4, 24), [255, 255, 255, 255]);
    assert_eq!(px(&base, VIRTUAL.x, 21, 24), [255, 255, 255, 255]);
    assert_eq!(
        px(&base, VIRTUAL.x, 14, 24),
        [0, 255, 0, 255],
        "mirrored: green on the left"
    );
    assert_eq!(px(&base, VIRTUAL.x, 0, 0), [51, 102, 204, 255], "clear color");

    // Windows that need bars on both axes at scales 2 and 3.
    for (scale, window) in [(1, VIRTUAL), (2, UVec2::new(140, 80)), (3, UVec2::new(200, 110))] {
        let pp = PixelPerfect {
            virtual_size: VIRTUAL,
            bar_color: BAR,
        };
        let lb = pp.letterbox(window);
        assert_eq!(lb.scale, scale);
        let pixels = if scale == 1 {
            base.clone()
        } else {
            render(&device, window, center)
        };
        // Every virtual pixel is an exact scale×scale block; outside the letterbox only bars.
        for y in 0..window.y {
            for x in 0..window.x {
                let got = px(&pixels, window.x, x, y);
                let inside = x >= lb.x && y >= lb.y && x < lb.x + lb.width && y < lb.y + lb.height;
                let want = if inside {
                    px(&base, VIRTUAL.x, (x - lb.x) / scale, (y - lb.y) / scale)
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(got, want, "scale {scale}: pixel ({x}, {y})");
            }
        }
        check_golden(&device, &format!("pixel_perfect_x{scale}"), window, &pixels);
    }
}

#[test]
fn pixel_perfect_transparent_target_composites_alpha_once() {
    check_pixel_perfect_alpha(None, BAR, [128, 128, 128], [188, 188, 188]);
}

#[test]
fn pixel_perfect_translucent_clear_is_premultiplied() {
    check_pixel_perfect_alpha(
        Some([1.0, 0.0, 0.0, 0.5]),
        [0.0, 0.0, 1.0, 1.0],
        [191, 128, 191],
        [225, 188, 225],
    );
}

fn check_pixel_perfect_alpha(clear: Option<[f32; 4]>, bar_color: [f32; 4], unorm: [u8; 3], srgb: [u8; 3]) {
    let device = device();
    for (format, expected) in [(FORMAT, unorm), (wgpu::TextureFormat::Rgba8UnormSrgb, srgb)] {
        let atlas = LoadedAtlas::new(SpriteAtlas::single(
            "white",
            RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![255; 4],
            },
        ));
        let mut frame = Frame2d::new();
        let mut sprite = Sprite::named("a", "white");
        sprite.size = Some([2.0; 2]);
        assert!(frame.push_sprite(
            &atlas,
            &sprite,
            &Affine3A::from_translation(Vec3::new(2.0, 2.0, 0.0)),
            [1.0, 1.0, 1.0, 0.5],
        ));
        let settings = DrawSettings {
            camera: OrthoCamera {
                center: Vec2::splat(2.0),
                ..Default::default()
            },
            pixel_perfect: Some(PixelPerfect {
                virtual_size: UVec2::splat(4),
                bar_color,
            }),
            clear,
            snap: true,
        };
        let pixels =
            render_offscreen(&device, frame.prepare(), settings, 10, 10, format, [0.0; 4]).expect("renders");
        let got = px(&pixels, 10, 4, 4);
        for (channel, want) in got[..3].iter().zip(expected) {
            assert!(
                channel.abs_diff(want) <= 2,
                "{format:?}: {got:?}, expected {expected:?}"
            );
        }
        assert_eq!(got[3], 255, "opaque bars keep the composite opaque");
        // The uncovered virtual pixel must also composite the clear over the bars once.
        let background = px(&pixels, 10, 1, 1);
        let expected_background = if clear.is_some() {
            if format.is_srgb() {
                [188, 0, 188, 255]
            } else {
                [128, 0, 128, 255]
            }
        } else {
            [0, 0, 0, 255]
        };
        for (channel, want) in background.iter().zip(expected_background) {
            assert!(
                channel.abs_diff(want) <= 2,
                "{format:?}: background {background:?}"
            );
        }
    }
}

#[test]
fn snapping_keeps_scrolling_sprites_crisp() {
    let device = device();
    let window = UVec2::new(128, 72);
    let crisp = render(&device, window, Vec2::new(32.0, 18.0));
    // A camera 0.3 px off snaps back to the same whole-pixel view.
    let off = render(&device, window, Vec2::new(32.3, 17.8));
    assert_eq!(crisp, off);
    // A whole-pixel scroll moves the image by exactly `scale` window pixels.
    let moved = render(&device, window, Vec2::new(33.0, 18.0));
    for y in 0..window.y {
        for x in 0..window.x - 2 {
            assert_eq!(px(&moved, window.x, x, y), px(&crisp, window.x, x + 2, y));
        }
    }
}

#[test]
fn nine_slice_corners_stay_fixed_and_the_centre_stretches() {
    let device = device();
    let base = render(&device, VIRTUAL, Vec2::new(32.0, 18.0));
    // Panel: world (28, 16) size 30×14 → virtual columns 28..58, rows (36-30)=6..20 from the top.
    // The top-left source texel is magenta and stays one pixel; the 3-pixel border stays 3 wide.
    assert_eq!(px(&base, VIRTUAL.x, 28, 6), [255, 0, 255, 255]);
    assert_eq!(px(&base, VIRTUAL.x, 29, 6), [120, 60, 20, 255]);
    assert_eq!(px(&base, VIRTUAL.x, 30, 8), [120, 60, 20, 255]);
    assert_eq!(
        px(&base, VIRTUAL.x, 31, 9),
        [240, 220, 180, 255],
        "center starts after 3 px"
    );
    assert_eq!(px(&base, VIRTUAL.x, 54, 16), [240, 220, 180, 255]);
    assert_eq!(
        px(&base, VIRTUAL.x, 55, 17),
        [120, 60, 20, 255],
        "right/bottom border is 3 px"
    );
    assert_eq!(px(&base, VIRTUAL.x, 57, 19), [120, 60, 20, 255]);
    assert_eq!(
        px(&base, VIRTUAL.x, 58, 19),
        [51, 102, 204, 255],
        "outside the panel"
    );
    // The dot (z 2) draws over the panel (z 0): centred at (43, 23) → columns 41..45, rows 11..15.
    assert_eq!(px(&base, VIRTUAL.x, 42, 12), [255, 255, 255, 255]);
}

#[test]
fn direct_mode_draws_over_the_frame_without_a_clear() {
    let device = device();
    let mut settings = settings(Vec2::new(32.0, 18.0));
    settings.pixel_perfect = None;
    settings.clear = None;
    let pixels = render_offscreen(
        &device,
        scene(&atlas()).prepare(),
        settings,
        64,
        36,
        FORMAT,
        [1.0, 0.0, 1.0, 1.0],
    )
    .expect("renders");
    assert_eq!(
        px(&pixels, 64, 0, 0),
        [255, 0, 255, 255],
        "the frame's clear survives"
    );
    assert_eq!(px(&pixels, 64, 4, 24), [255, 255, 255, 255]);
}

#[test]
fn direct_mode_rejects_non_finite_instance_alpha() {
    let device = device();
    let atlas = atlas();
    for alpha in [f32::INFINITY, f32::NAN, f32::NEG_INFINITY] {
        let mut frame = Frame2d::new();
        assert!(frame.push_sprite(
            &atlas,
            &Sprite::named("a", "dot"),
            &Affine3A::from_translation(Vec3::new(2.0, 2.0, 0.0)),
            [1.0; 4],
        ));
        // PreparedFrame is public: bypass extraction to exercise the shader's defense too.
        let mut prepared = frame.prepare();
        prepared.instances[0].color[3] = alpha;
        let mut settings = settings(Vec2::splat(2.0));
        settings.pixel_perfect = None;
        settings.clear = None;
        let pixels = render_offscreen(&device, prepared, settings, 4, 4, FORMAT, [1.0, 0.0, 1.0, 1.0])
            .expect("renders");
        assert_eq!(px(&pixels, 4, 2, 2), [255, 0, 255, 255], "alpha {alpha}");
    }
}

#[test]
fn public_sprite_extraction_normalizes_tint_alpha() {
    check_public_tint_alpha(false);
}

#[test]
fn public_tilemap_extraction_normalizes_tint_alpha() {
    check_public_tint_alpha(true);
}

fn check_public_tint_alpha(is_tilemap: bool) {
    use regia_2d::TileMap;
    use regia_2d::sprite::LoadedTileMap;
    use regia_2d::tilemap::{COOKED_TILEMAP_VERSION, CookedTileMap, MapLayer, TileMapAsset};

    let map = TileMapAsset::from_ron(
        r#"(
        version: 2, width: 1, height: 1, tile_width: 1, tile_height: 1,
        tilesets: [(name: "white", first_gid: 1, image: "white.png", image_width: 1,
                    image_height: 1, tile_width: 1, tile_height: 1, columns: 1, tile_count: 1)],
        layers: [Tiles((name: "ground", data: [1]))],
    )"#,
    )
    .expect("valid map");
    let mut loaded = LoadedTileMap::new(CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map,
        images: vec![RgbaImage {
            width: 1,
            height: 1,
            rgba8: vec![255; 4],
        }],
    });
    let atlas = atlas();
    for (alpha, opacity, sprite_alpha, tile_alpha) in [
        (2.0, 1.0, Some(1.0), Some(1.0)),
        (2.0, 0.25, Some(1.0), Some(0.5)),
        (0.5, 0.5, Some(0.5), Some(0.25)),
        (0.0, 1.0, None, None),
        (-1.0, 1.0, None, None),
        (f32::NAN, 1.0, None, None),
        (f32::INFINITY, 1.0, None, None),
        (f32::NEG_INFINITY, 1.0, None, None),
    ] {
        let tint = [1.0, 1.0, 1.0, alpha];
        let mut frame = Frame2d::new();
        let expected = if is_tilemap {
            if let MapLayer::Tiles(layer) = &mut loaded.map.layers[0] {
                layer.opacity = opacity;
            }
            frame.push_tilemap(&loaded, &TileMap::new("white"), &Affine3A::IDENTITY, tint);
            tile_alpha
        } else {
            assert!(frame.push_sprite(&atlas, &Sprite::named("a", "dot"), &Affine3A::IDENTITY, tint));
            sprite_alpha
        };
        let actual: Vec<f32> = frame.prepare().instances.iter().map(|q| q.color[3]).collect();
        assert_eq!(
            actual,
            expected.into_iter().collect::<Vec<_>>(),
            "alpha {alpha}, opacity {opacity}"
        );
    }
}

#[test]
fn rebased_wide_tilemap_renders_neighboring_cells_separately() {
    use regia_2d::TileMap;
    use regia_2d::camera::Rect2;
    use regia_2d::sprite::LoadedTileMap;
    use regia_2d::tilemap::{
        COOKED_TILEMAP_VERSION, CookedTileMap, MapLayer, Orientation, TILEMAP_FORMAT_VERSION, TileLayer,
        TileMapAsset, TilesetDef,
    };

    let mut data = vec![0; 20_000_000];
    data[16_777_216..=16_777_217].copy_from_slice(&[1, 2]);
    let map = TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width: 20_000_000,
        height: 1,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![TilesetDef {
            name: "colors".into(),
            first_gid: 1,
            image: "colors.png".into(),
            image_width: 32,
            image_height: 16,
            tile_width: 16,
            tile_height: 16,
            columns: 2,
            tile_count: 2,
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
            properties: Default::default(),
        })],
        properties: Default::default(),
    };
    assert_eq!(map.tile_world_rect(16_777_217, 0).0[0] - 268_435_456.0, 16.0);
    let mut rgba8 = Vec::new();
    for _ in 0..16 {
        for x in 0..32 {
            rgba8.extend_from_slice(if x < 16 {
                &[255, 0, 0, 255]
            } else {
                &[0, 255, 0, 255]
            });
        }
    }
    let cooked = CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map,
        images: vec![RgbaImage {
            width: 32,
            height: 16,
            rgba8,
        }],
    };
    cooked.validate().expect("valid wide map and tileset");
    let loaded = LoadedTileMap::new(cooked);
    let mut frame = Frame2d::new();
    frame.visible = Some(Rect2::new(0.0, 0.0, 32.0, 16.0));
    assert_eq!(
        frame.push_tilemap(
            &loaded,
            &TileMap::new("wide"),
            &Affine3A::from_translation(Vec3::new(-268_435_456.0, 0.0, 0.0)),
            [1.0; 4],
        ),
        2
    );
    let settings = DrawSettings {
        camera: OrthoCamera {
            center: Vec2::new(16.0, 8.0),
            ..Default::default()
        },
        clear: Some(BAR),
        ..Default::default()
    };
    let pixels =
        render_offscreen(&device(), frame.prepare(), settings, 32, 16, FORMAT, [0.0; 4]).expect("renders");
    assert_eq!(px(&pixels, 32, 8, 8), [255, 0, 0, 255]);
    assert_eq!(px(&pixels, 32, 24, 8), [0, 255, 0, 255]);
}

#[test]
fn srgb_targets_round_trip_texels_exactly() {
    let device = device();
    let mut settings = settings(Vec2::new(32.0, 18.0));
    settings.pixel_perfect = None;
    let pixels = render_offscreen(
        &device,
        scene(&atlas()).prepare(),
        settings,
        64,
        36,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        [0.0, 0.0, 0.0, 1.0],
    )
    .expect("renders");
    assert_eq!(px(&pixels, 64, 30, 8), [120, 60, 20, 255]);
    assert_eq!(px(&pixels, 64, 31, 9), [240, 220, 180, 255]);
}

#[test]
fn plugin_extracts_sprites_tilemaps_and_animations() {
    use regia_2d::animation::{AnimatedSprite, Animation};
    use regia_2d::{Sprite2dPlugin, SpriteAssets, SpritePass};
    use regia_app::prelude::*;
    use regia_scene::Transform;

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(125),
    ));
    {
        let mut assets = app.world_mut().resource_mut::<SpriteAssets>();
        let LoadedAtlas { mut atlas, pages } = atlas();
        atlas.pages = pages.iter().map(|p| p.image.clone()).collect();
        assets.insert_atlas("a", atlas);
    }
    app.world_mut().spawn((
        Sprite::named("a", "hero"),
        Transform::from_translation(Vec3::new(4.0, 4.0, 0.0)),
    ));
    let walker = app
        .world_mut()
        .spawn((
            Sprite::rect("a", [0, 0, 4, 4]),
            AnimatedSprite::new(4, 4, &[Animation::new("walk", 0, 3, 8.0)], true),
            Transform::default(),
        ))
        .id();
    let mut hidden = Sprite::named("a", "dot");
    hidden.visible = false;
    app.world_mut().spawn((hidden, Transform::default()));
    app.world_mut()
        .spawn((Sprite::named("missing-atlas", "x"), Transform::default()));

    app.update();
    let stats = app.world().resource::<SpritePass>().stats();
    assert_eq!((stats.sprites, stats.instances, stats.batches), (2, 2, 1));
    // 125 ms per frame at 8 fps: one frame per update.
    app.update();
    let animated = app.world().get::<AnimatedSprite>(walker).expect("animated");
    assert!(
        animated.frame_index() >= 1,
        "advanced to frame {}",
        animated.frame_index()
    );
    let sprite = app.world().get::<Sprite>(walker).expect("sprite");
    assert_eq!(
        sprite.source,
        regia_2d::SpriteSource::Rect {
            rect: animated.frame().source_rect
        }
    );
}

/// Two schedules totalling one second at 3 fps must leave the same `Sprite.source`: four 250 ms
/// frames and sixty 16.667 ms frames slice the same second differently, and the sprite-sheet
/// clock must not care (determinism audit 11-08 M1). Three steps of a 4-frame loop land on
/// frame 3 either way.
#[test]
fn sprite_source_matches_across_frame_slicings() {
    use regia_2d::Sprite2dPlugin;
    use regia_2d::animation::{AnimatedSprite, Animation};
    use regia_app::prelude::*;
    use regia_scene::Transform;
    use std::time::Duration;

    let run = |step: Duration, updates: usize| {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
        let id = app
            .world_mut()
            .spawn((
                Sprite::rect("a", [0, 0, 4, 4]),
                AnimatedSprite::new(4, 4, &[Animation::new("walk", 0, 4, 3.0)], true),
                Transform::default(),
            ))
            .id();
        for _ in 0..updates {
            app.update();
        }
        app.world().get::<Sprite>(id).expect("sprite").source.clone()
    };
    let quarter_second_frames = run(Duration::from_millis(250), 4);
    let sixtieth_second_frames = run(Duration::from_micros(16_667), 60);
    assert_eq!(
        quarter_second_frames, sixtieth_second_frames,
        "the sprite source must not depend on how the second is sliced"
    );
    assert_eq!(
        quarter_second_frames,
        regia_2d::SpriteSource::Rect { rect: [12, 0, 4, 4] },
        "three steps of a 4-frame 3 fps loop land on frame 3"
    );
}

#[test]
fn equal_z_sprites_keep_their_spawn_order_after_a_despawn() {
    use regia_2d::{Sprite2dPlugin, SpriteAssets, SpritePass};
    use regia_app::prelude::*;
    use regia_scene::Transform;

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    {
        let mut assets = app.world_mut().resource_mut::<SpriteAssets>();
        let LoadedAtlas { mut atlas, pages } = atlas();
        atlas.pages = pages.iter().map(|p| p.image.clone()).collect();
        assets.insert_atlas("a", atlas);
    }
    let mut ids = Vec::new();
    for x in [10.0, 20.0, 30.0] {
        ids.push(
            app.world_mut()
                .spawn((
                    Sprite::named("a", "hero"),
                    Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                ))
                .id(),
        );
    }
    // Removing the first sprite swaps the last table row into its hole.
    app.world_mut().despawn(ids[0]);
    app.world_mut().spawn((
        Sprite::named("a", "hero"),
        Transform::from_translation(Vec3::new(40.0, 0.0, 0.0)),
    ));
    app.update();
    let pass = app.world().resource::<SpritePass>();
    let state = pass.0.lock().expect("pass state");
    let xs: Vec<f32> = state.prepared.instances.iter().map(|i| i.origin[0]).collect();
    assert_eq!(xs.len(), 3);
    assert!(
        xs.windows(2).all(|w| w[0] < w[1]),
        "equal-z sprites must draw in spawn order, got origins {xs:?}"
    );
}

/// A renderer keeps one instance buffer and one view bind across draws. A larger frame may
/// allocate the instance buffer again; a repeat of that frame does not. Pixel-perfect mode
/// adds one blit bind, once.
///
/// The old `draw` created a uniform and, when the frame was not empty, a vertex buffer on
/// every call: two draws were 2 instance buffers and 2 view binds.
#[test]
fn retained_buffers_are_created_once_per_renderer() {
    use regia_2d::batch::{PreparedFrame, SpriteInstance};
    use regia_2d::render::SpriteRenderer;

    let device = device();
    let prepared = scene(&atlas()).prepare();
    assert!(!prepared.instances.is_empty());
    let settings = DrawSettings {
        camera: OrthoCamera {
            center: Vec2::new(32.0, 18.0),
            pixels_per_unit: 1.0,
            rotation: 0.0,
        },
        pixel_perfect: None,
        snap: false,
        clear: Some(SKY),
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sprite2d retain test"),
        size: wgpu::Extent3d {
            width: 64,
            height: 36,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = SpriteRenderer::new(&device);

    let submit = |renderer: &mut SpriteRenderer, frame: &PreparedFrame, draw_settings: &DrawSettings| {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sprite2d retain"),
            });
        renderer.draw(
            &device,
            &mut encoder,
            &target,
            FORMAT,
            64,
            36,
            frame,
            draw_settings,
        );
        device.queue.submit([encoder.finish()]);
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };

    submit(&mut renderer, &prepared, &settings);
    assert_eq!(renderer.instance_buffer_creates(), 1);
    assert_eq!(renderer.view_bind_creates(), 1);
    submit(&mut renderer, &prepared, &settings);
    assert_eq!(
        renderer.instance_buffer_creates(),
        1,
        "a second draw used to create another instance buffer"
    );
    assert_eq!(
        renderer.view_bind_creates(),
        1,
        "a second draw used to create another view bind"
    );

    let instance_bytes = std::mem::size_of::<SpriteInstance>();
    let first_bytes = instance_bytes * prepared.instances.len();
    let cap = first_bytes
        .max(4)
        .checked_next_power_of_two()
        .unwrap_or(first_bytes)
        .max(256);
    let need = cap / instance_bytes + 1;
    let mut grown = prepared.clone();
    let proto = grown.instances[0];
    grown.instances.resize(need, proto);
    if let Some((_, range)) = grown.batches.last_mut() {
        range.end = grown.instances.len() as u32;
    }
    submit(&mut renderer, &grown, &settings);
    assert_eq!(
        renderer.instance_buffer_creates(),
        2,
        "growing past the retained power-of-two buffer allocates once"
    );
    submit(&mut renderer, &grown, &settings);
    assert_eq!(renderer.instance_buffer_creates(), 2);

    let pixel_perfect = DrawSettings {
        pixel_perfect: Some(PixelPerfect::new(64, 36)),
        ..settings
    };
    submit(&mut renderer, &prepared, &pixel_perfect);
    assert_eq!(
        renderer.view_bind_creates(),
        2,
        "pixel-perfect mode adds one blit bind"
    );
    submit(&mut renderer, &prepared, &pixel_perfect);
    assert_eq!(renderer.view_bind_creates(), 2);
    assert_eq!(renderer.instance_buffer_creates(), 2);
}
