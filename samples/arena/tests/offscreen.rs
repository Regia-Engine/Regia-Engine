// SPDX-License-Identifier: MIT-0
//! Offscreen evidence (`docs/specs/14` M20 `samples/arena`): render the arena headless, composite
//! the HUD, and check the pixels — floor and bots readable, first person from the pawn's eyes,
//! not clipped to white. With `REGIA_WRITE_EVIDENCE=1` the PNGs and `metrics.txt` land in
//! `target/evidence/m20/arena/` (never in `docs/`, which holds the committed goldens).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use regia_app::{App, MinimalPlugins};
use regia_audio::AudioSettings;
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::CameraRig;
use regia_framework::settings::SettingsPath;
use regia_render::{Camera, ClearMode, ExtractedFrame, FrameRenderer, FrameScene, RenderDevice};
use regia_scene::{GlobalTransform, Transform};
use regia_ui::SharedUiRenderer;

use arena::camera::EXPOSURE_EV;
use arena::{ArenaConfig, ArenaSample, MatchRng, Pawn};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// Builds a headless arena app with the offscreen render path (lavapipe).
fn arena_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ArenaSample,
    ));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: true,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 180.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(0xA11CE));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

/// The directory this test writes its evidence PNGs and `metrics.txt` to, or
/// `None` when evidence is not requested.
///
/// Evidence is a review artifact, not a committed golden. `cargo test --workspace`
/// must never rewrite the committed files under `docs/evidence/`, so the write is
/// gated behind `REGIA_WRITE_EVIDENCE=1` and lands in `target/evidence/` (git-ignored).
/// The pixel assertions run regardless; only the side-effect write is opt-in.
fn evidence_dir() -> Option<PathBuf> {
    if std::env::var_os("REGIA_WRITE_EVIDENCE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return None;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/evidence/m20/arena");
    std::fs::create_dir_all(&path).expect("evidence dir");
    Some(path)
}

/// Writes an RGBA8 buffer to a PNG file.
fn write_png(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create evidence dir");
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

/// Spawns an overhead camera high enough that the whole floor fits (view order 1).
/// No [`CameraRig`], so the framework camera system leaves it alone.
fn spawn_overhead_camera(app: &mut App) {
    let transform = Transform {
        translation: Vec3::new(0.0, 28.0, 0.0),
        rotation: Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        scale: Vec3::ONE,
    };
    app.world_mut().spawn((
        regia_scene::components::Name::new("overhead"),
        transform,
        GlobalTransform(transform.compute_affine()),
        Camera {
            fov_y: 60.0_f32.to_radians(),
            near: 0.1,
            far: 200.0,
            exposure_ev: Some(EXPOSURE_EV),
            clear: ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
            order: 1,
            ..Camera::default()
        },
    ));
}

/// Puts the local pawn on the south side, looking −Z through the arena, and parks three bots
/// in that view so the capture shows distinct bodies. Aims the rig with the pawn.
fn look_through_the_arena(app: &mut App) {
    {
        let world = app.world_mut();
        let mut pawns = world.query::<(&Pawn, &mut Transform)>();
        let mut placed = 0u8;
        for (pawn, mut transform) in pawns.iter_mut(world) {
            if pawn.is_bot {
                // Slots 1..3 cycle red, green, yellow (`meshes::BOT_COLORS`).
                let spot = match placed {
                    0 => Vec3::new(-2.2, arena::pawn::PAWN_EYE_HEIGHT, 1.5),
                    1 => Vec3::new(0.2, arena::pawn::PAWN_EYE_HEIGHT, -1.2),
                    2 => Vec3::new(2.4, arena::pawn::PAWN_EYE_HEIGHT, 0.4),
                    _ => continue,
                };
                placed += 1;
                transform.translation = spot;
                continue;
            }
            transform.translation = Vec3::new(0.0, arena::pawn::PAWN_EYE_HEIGHT, 6.0);
            transform.rotation = Quat::IDENTITY;
        }
    }
    {
        let world = app.world_mut();
        let mut rigs = world.query::<&mut CameraRig>();
        for mut rig in rigs.iter_mut(world) {
            rig.yaw = 0.0;
            rig.pitch = 0.0;
        }
    }
}

/// Renders `scene`, draws the extracted HUD on top, and reads RGBA8 pixels back.
fn composite(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    ui: &SharedUiRenderer,
) -> regia_render::ForwardResult {
    let target = renderer
        .begin_offscreen(device, scene, wgpu::TextureFormat::Rgba8UnormSrgb)
        .expect("offscreen scene");
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("arena hud"),
        });
    {
        let mut guard = ui.lock();
        guard.draw(
            device,
            &mut encoder,
            &target.view,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            scene.width,
            scene.height,
        );
    }
    device.queue.submit([encoder.finish()]);
    renderer.read_offscreen(device, target).expect("readback")
}

struct ImageStats {
    white_frac: f32,
    mean: f32,
    bright_top: u32,
    dark_top: u32,
    hue_buckets: u32,
    mid_gray: u32,
    bot_hues: u32,
}

/// Minimum pixels of a bot-only hue before that bot counts as visible.
const BOT_PIXEL_MIN: u32 = 8;

/// A bot-green pixel: green clearly dominates red and blue (slot 2 bot bodies). The fire
/// button (red) and the floor (blue-grey) cannot produce this.
fn is_bot_green(r: i32, g: i32, b: i32) -> bool {
    g > r + 30 && g > b + 30 && g > 60
}

/// A bot-yellow pixel: red and green both bright and close, blue far below (slot 3 bot bodies).
///
/// `r > 120`: at the sunny-day exposure (EV 14.5) and the corrected AgX, the yellow bot's
/// sunlit top face prints ≈ (140, 143, 66) in the overhead view. The old `r > 150` was met
/// only by the 1.5 EV overexposure the arena used to compensate the broken tonemap fit.
fn is_bot_yellow(r: i32, g: i32, b: i32) -> bool {
    r > 120 && g > 120 && b < 90 && (r - g).abs() < 70
}

/// Mean Rec.709 luma of `x0..x1`, `y0..y1`, and the fraction of pixels with every channel below 8.
fn band_luma(pixels: &[u8], width: u32, x0: u32, x1: u32, y0: u32, y1: u32) -> (f32, f32) {
    let px = pixels.as_chunks::<4>().0;
    let mut sum = 0.0f32;
    let mut dark = 0.0f32;
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in x0..x1.min(width) {
            let p = px[(y * width + x) as usize];
            sum += 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
            if p[0] < 8 && p[1] < 8 && p[2] < 8 {
                dark += 1.0;
            }
            n += 1.0;
        }
    }
    let n = n.max(1.0);
    (sum / n, dark / n)
}

fn measure(pixels: &[u8], width: u32, height: u32) -> ImageStats {
    let px = pixels.as_chunks::<4>().0;
    let mut white = 0u32;
    let mut sum = 0u64;
    let mut bright_top = 0u32;
    let mut dark_top = 0u32;
    let mut mid_gray = 0u32;
    let mut buckets = [0u32; 6];
    let mut bot_green = 0u32;
    let mut bot_yellow = 0u32;
    let top = 64u32.min(height);
    for (index, p) in px.iter().enumerate() {
        let y = (index as u32) / width;
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        sum += (r + g + b) as u64;
        if r >= 250 && g >= 250 && b >= 250 {
            white += 1;
        }
        if max - min < 28 && max > 45 && max < 170 {
            mid_gray += 1;
        }
        if y < top {
            if min >= 200 {
                bright_top += 1;
            }
            // The HUD panel is linear (0.05, 0.06, 0.08) at 92 % opacity: ≈ (63, 69, 79)
            // sRGB over black, up to ≈ (70, 80, 95) over the sky. The old `max <= 48`
            // could never match it and only counted the near-black background the
            // renderer used to clear to; first person now shows the sky there.
            if max <= 100 && max - min <= 32 {
                dark_top += 1;
            }
        }
        if max >= 70 && max - min >= 36 {
            let bucket = if r >= g && r >= b {
                if g > b + 25 { 3 } else { 0 } // yellow-ish vs red
            } else if g >= r && g >= b {
                if b > r + 20 { 4 } else { 1 } // cyan vs green
            } else if r > g + 20 {
                5 // magenta / purple
            } else {
                2 // blue
            };
            buckets[bucket] += 1;
        }
        if is_bot_green(r, g, b) {
            bot_green += 1;
        }
        if is_bot_yellow(r, g, b) {
            bot_yellow += 1;
        }
    }
    let n = px.len().max(1) as f32;
    let hue_buckets = buckets.iter().filter(|c| **c >= 12).count() as u32;
    let bot_hues = u32::from(bot_green >= BOT_PIXEL_MIN) + u32::from(bot_yellow >= BOT_PIXEL_MIN);
    ImageStats {
        white_frac: white as f32 / n,
        mean: sum as f32 / (n * 3.0),
        bright_top,
        dark_top,
        hue_buckets,
        mid_gray,
        bot_hues,
    }
}

/// Checks a capture is not clipped or flat, carries a HUD, and (when asked) shows a readable
/// floor and `want_bot_hues` distinct bot colours. Bot hues are counted only from green/yellow
/// pixels, which the red fire button and the blue-grey floor cannot produce.
fn assert_readable(name: &str, stats: &ImageStats, want_floor: bool, want_bot_hues: u32) {
    eprintln!(
        "{name}: white {:.1}%  mean {:.1}  bright_top {}  dark_top {}  hue_buckets {}  mid_gray {}  bot_hues {}",
        stats.white_frac * 100.0,
        stats.mean,
        stats.bright_top,
        stats.dark_top,
        stats.hue_buckets,
        stats.mid_gray,
        stats.bot_hues
    );
    assert!(
        stats.white_frac < 0.25,
        "{name} is clipped white ({:.1}%)",
        stats.white_frac * 100.0
    );
    assert!(
        stats.mean > 25.0 && stats.mean < 210.0,
        "{name} mean luminance {:.1} is flat black or flat white",
        stats.mean
    );
    assert!(
        stats.bright_top > 20,
        "{name} HUD glyphs missing (bright pixels in the top strip: {})",
        stats.bright_top
    );
    assert!(
        stats.dark_top > 40,
        "{name} HUD panel missing (dark pixels in the top strip: {})",
        stats.dark_top
    );
    if want_floor {
        assert!(
            stats.mid_gray > 400,
            "{name} floor is not a readable mid-tone ({} pixels)",
            stats.mid_gray
        );
    }
    if want_bot_hues > 0 {
        assert!(
            stats.bot_hues >= want_bot_hues,
            "{name} bots are not distinct colors ({} bot hues, want {want_bot_hues})",
            stats.bot_hues
        );
    }
}

#[test]
fn arena_renders_offscreen_not_black_with_bots_and_hud() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut app = arena_app();
    app.update();
    app.world_mut().write_message(regia_platform::WindowResized {
        window: regia_ecs::prelude::Entity::PLACEHOLDER,
        width: WIDTH,
        height: HEIGHT,
    });
    for _ in 0..45 {
        app.update();
    }
    look_through_the_arena(&mut app);
    for _ in 0..3 {
        app.update();
    }
    spawn_overhead_camera(&mut app);
    app.update();

    let frame = app.world().resource::<ExtractedFrame>().clone();
    assert!(
        frame.views.len() >= 2,
        "first-person and overhead views, got {}",
        frame.views.len()
    );

    // Extraction storage follows entity identity, which can differ from camera draw order.
    // Use the renderer's ordered view API and retain its raw indices for frame_scene.
    let window_views = frame.window_views();
    assert_eq!(window_views.len(), 2, "first-person and overhead window views");
    let (first_index, first_view) = &window_views[0];
    let (overhead_index, overhead_view) = &window_views[1];
    assert_eq!(first_view.order, 0, "first-person draw order");
    assert_eq!(overhead_view.order, 1, "overhead draw order");

    let player_eye = {
        let world = app.world_mut();
        let mut pawns = world.query::<(&Pawn, &Transform)>();
        pawns
            .iter(world)
            .find(|(pawn, _)| !pawn.is_bot)
            .map(|(_, transform)| transform.translation)
            .expect("local pawn")
    };
    let eye = Vec3::from_array(first_view.camera.eye);
    let look = Vec3::from_array(first_view.camera.target) - eye;
    assert!(
        (eye - player_eye).length() < 0.75,
        "first person is at the pawn's eyes: camera {eye:?} pawn {player_eye:?}"
    );
    assert!(
        look.z < -0.5,
        "first person looks through the arena (−Z), forward {look:?}"
    );

    let floor_meshes = frame
        .meshes
        .iter()
        .filter(|mesh| {
            let c = mesh.material.base_color;
            (c[0] - 0.30).abs() < 0.05 && (c[1] - 0.34).abs() < 0.05 && (c[2] - 0.38).abs() < 0.05
        })
        .count();
    assert!(floor_meshes >= 1, "the floor mesh is in the draw list");

    let ui = app.world().resource::<SharedUiRenderer>().clone();
    let mut renderer = FrameRenderer::new();
    let first = composite(
        &mut renderer,
        &device,
        &frame.frame_scene(*first_index, WIDTH, HEIGHT),
        &ui,
    );
    // `draw` consumes the quad list. Refill it for the overhead composite.
    regia_ui::extract::extract_ui(app.world_mut());
    let ui = app.world().resource::<SharedUiRenderer>().clone();
    let overhead = composite(
        &mut renderer,
        &device,
        &frame.frame_scene(*overhead_index, WIDTH, HEIGHT),
        &ui,
    );

    let fp_stats = measure(&first.pixels, first.width, first.height);
    let oh_stats = measure(&overhead.pixels, overhead.width, overhead.height);
    // First-person must show bot bodies (green/yellow, which the fire button and floor cannot
    // produce); the overhead shot must show two distinct bots.
    assert_readable("first_person", &fp_stats, false, 1);
    assert_readable("overhead", &oh_stats, true, 2);
    // Overhead looks straight down from 28 m. Past the 12 m walls the ray misses the
    // floor and meets the planet: that corner used to be the forward clear (solid black).
    // The HUD is the top-left panel and the fire button is the bottom-right, so the
    // top-right strip is only sky.
    let (void_luma, void_dark) = band_luma(
        &overhead.pixels,
        overhead.width,
        overhead.width.saturating_sub(48),
        overhead.width.saturating_sub(8),
        8,
        48,
    );
    assert!(
        void_luma > 18.0 && void_dark < 0.05,
        "past the floor the view is black: luma {void_luma:.1}, dark fraction {void_dark:.2}"
    );

    if let Some(dir) = evidence_dir() {
        write_png(
            &dir.join("arena_first_person.png"),
            first.width,
            first.height,
            &first.pixels,
        );
        write_png(
            &dir.join("arena_overhead.png"),
            overhead.width,
            overhead.height,
            &overhead.pixels,
        );
        let metrics = format!(
            "first_person {WIDTH}x{HEIGHT} white {:.3} mean {:.1} bright_top {} dark_top {} bot_hues {}\n\
             overhead {WIDTH}x{HEIGHT} white {:.3} mean {:.1} bright_top {} dark_top {} hues {} mid {} bot_hues {}\n\
             eye {eye:?} pawn {player_eye:?} forward {look:?}\n",
            fp_stats.white_frac,
            fp_stats.mean,
            fp_stats.bright_top,
            fp_stats.dark_top,
            fp_stats.bot_hues,
            oh_stats.white_frac,
            oh_stats.mean,
            oh_stats.bright_top,
            oh_stats.dark_top,
            oh_stats.hue_buckets,
            oh_stats.mid_gray,
            oh_stats.bot_hues,
        );
        std::fs::write(dir.join("metrics.txt"), metrics).expect("metrics");
    }
}
