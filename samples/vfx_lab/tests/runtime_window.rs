// SPDX-License-Identifier: MIT-0
//! Round-3 window-mode regression test: renders the **window-mode app** (`WindowedVfxLab`,
//! the same plugin set `vfx_lab --window` runs) headless through the same offscreen path
//! `render_windows` uses for hidden-window screenshots (`begin_offscreen` on the extracted
//! frame) and asserts the captured frame contains all three scene layers:
//!
//! - the lit ground (green-ish pixels in the lower half),
//! - the sky (blue-ish pixels in the upper band),
//! - the effect (warm muzzle-flash pixels),
//!
//! with the effect covering < 40 % of the frame (the round-2 defect: a near-black frame
//! with oversized particles filling the screen — camera/extraction broken, scene absent).
//!
//! `REGIA_EVIDENCE_DIR=<dir>` additionally writes the captured frame as
//! `<dir>/runtime_window.png` (the `docs/evidence/m14` artifact).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia::platform::{ButtonState, KeyboardInput, MouseButton, MouseButtonInput, MouseMotion, MouseWheel};
use regia::prelude::*;
use regia_render::{ExtractedFrame, RenderDevice, RuntimeRenderer};
use vfx_lab::window::WindowedVfxLab;

/// The window-mode app without the platform runner: the game plugins, the render
/// extraction in `Render` (after `PostUpdate` transform propagation, as in the real
/// schedule), a headless render device, and a deterministic 60 Hz clock.
fn window_app() -> App {
    let mut app = App::new();
    app.add_plugins(WindowedVfxLab {
        exit_after_frames: 0,
        screenshot: None,
    });
    // Platform messages the orbit control reads (headless: none are ever sent).
    app.add_message::<MouseMotion>()
        .add_message::<MouseButtonInput>()
        .add_message::<MouseWheel>()
        .add_message::<KeyboardInput>();
    // The render side of the runtime path, minus window surfaces: extraction runs in
    // `Render`; the test renders the extracted frame offscreen exactly like
    // `render_windows` serves hidden-window agent screenshots.
    app.insert_resource(RenderDevice::headless().expect("headless GPU (lavapipe)"))
        .init_resource::<ExtractedFrame>()
        .init_resource::<RuntimeRenderer>()
        .init_resource::<regia_render::ScreenshotRequests>()
        .add_systems(Render, regia_render::extract_frame);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

/// Renders the current extracted frame offscreen (the hidden-screenshot path).
///
/// The target is `Rgba8UnormSrgb`, matching a window surface: hidden screenshots
/// pick that format when the surface is sRGB.
fn capture(app: &mut App, width: u32, height: u32) -> Vec<u8> {
    let mut runtime = app
        .world_mut()
        .remove_resource::<RuntimeRenderer>()
        .expect("runtime renderer");
    let device = app.world().resource::<RenderDevice>().clone();
    let scene = app
        .world()
        .resource::<ExtractedFrame>()
        .frame_scene(0, width, height);
    let target = runtime
        .0
        .begin_offscreen(&device, &scene, wgpu::TextureFormat::Rgba8UnormSrgb)
        .expect("offscreen render");
    let shot = runtime
        .0
        .read_offscreen(&device, target)
        .expect("offscreen readback");
    app.world_mut().insert_resource(runtime);
    shot.pixels
}

/// Frame layer measurements for the assertions (sRGB bytes).
#[derive(Debug)]
struct Bands {
    /// Fraction of the upper band (top third) that is sky-blue.
    sky: f32,
    /// Fraction of the lower half that is ground-green.
    ground: f32,
    /// Fraction of the whole frame that is warm effect-lit pixels.
    effect: f32,
    /// Mean luminance of the whole frame.
    mean_luma: f32,
    /// Fraction of the zenith band (top seventh) whose Chebyshev distance from
    /// the modal sky colour exceeds 12. The round-5 window PNG is ~65% here
    /// (full-width IBL lobe); a flat emissive sky is a few percent at most.
    zenith_far: f32,
    /// Distinct sky colours on one top-band row, outside warm effect pixels.
    zenith_row_colors: u32,
}

/// Fraction of pixels that are warm (red ≥ green, red + green well above blue) or
/// white-hot (every channel ≥ 230) in `after` and differ from `before` by more than 30
/// levels summed over RGB.
fn effect_added(before: &[u8], after: &[u8]) -> f32 {
    let a = before.as_chunks::<4>().0;
    let b = after.as_chunks::<4>().0;
    let added = a
        .iter()
        .zip(b)
        .filter(|(old, new)| {
            let changed = (0..3).map(|c| old[c].abs_diff(new[c]) as u32).sum::<u32>() > 30;
            let warm =
                f32::from(new[0]) + f32::from(new[1]) > 2.0 * f32::from(new[2]) + 40.0 && new[0] >= new[1];
            let hot = new[0] >= 230 && new[1] >= 230 && new[2] >= 230;
            changed && (warm || hot)
        })
        .count();
    added as f32 / b.len().max(1) as f32
}

fn measure(pixels: &[u8], width: u32) -> Bands {
    let px = pixels.as_chunks::<4>().0;
    let height = (px.len() / width as usize) as u32;
    let luma =
        |p: [u8; 4]| (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0;
    let (mut sky_n, mut sky_hit, mut ground_n, mut ground_hit, mut effect, mut luma_sum) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0.0f32);
    // Modal sky colour: the most common sky-blue texel in the upper third.
    let mut sky_counts: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
    let zenith_rows = (height / 7).max(1);
    let row_y = height / 16;
    for (i, p) in px.iter().enumerate() {
        let y = (i / width as usize) as u32;
        let l = luma(*p);
        luma_sum += l;
        // Red at least as strong as green: muzzle-flash orange, not the sunlit grass,
        // which the corrected display transform prints bright enough to pass the
        // r + g > 2b test on its own (it counted ~60 % of the frame as "effect").
        let warm =
            f32::from(p[0]) + f32::from(p[1]) > 2.0 * f32::from(p[2]) + 40.0 && p[0] >= p[1] && l > 0.06;
        // Warm effect pixel: muzzle-flash orange, lit (the ground is green, the sky blue).
        if warm {
            effect += 1;
        }
        let sky_blue = f32::from(p[2]) > f32::from(p[0]) + 12.0 && p[2] > 70;
        if y < height / 3 {
            sky_n += 1;
            // Sky-blue: blue dominant and clearly not dark.
            if sky_blue {
                sky_hit += 1;
                *sky_counts.entry([p[0], p[1], p[2]]).or_insert(0) += 1;
            }
        } else if y > height / 2 {
            ground_n += 1;
            // Ground-green: green dominant over red and blue, and visible.
            if f32::from(p[1]) > f32::from(p[0]) + 4.0 && f32::from(p[1]) > f32::from(p[2]) + 4.0 && l > 0.03
            {
                ground_hit += 1;
            }
        }
    }
    let modal = sky_counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(color, _)| *color)
        .unwrap_or([0, 0, 0]);
    let chebyshev = |p: [u8; 4]| {
        (i32::from(p[0]) - i32::from(modal[0]))
            .abs()
            .max((i32::from(p[1]) - i32::from(modal[1])).abs())
            .max((i32::from(p[2]) - i32::from(modal[2])).abs())
    };
    let (mut zenith_n, mut zenith_far) = (0u32, 0u32);
    let mut row_colors: std::collections::HashSet<[u8; 3]> = std::collections::HashSet::new();
    for (i, p) in px.iter().enumerate() {
        let y = (i / width as usize) as u32;
        if y < zenith_rows {
            zenith_n += 1;
            // 12 levels: the round-5 PNG's zenith lobe sits ~30–80 away from the
            // flat emissive sky; a one-level quantize stays inside.
            if chebyshev(*p) > 12 {
                zenith_far += 1;
            }
        }
        // One top-band row, columns that are sky rather than the effect.
        if y == row_y {
            let l = luma(*p);
            let warm = f32::from(p[0]) + f32::from(p[1]) > 2.0 * f32::from(p[2]) + 40.0 && l > 0.06;
            let sky_blue = f32::from(p[2]) > f32::from(p[0]) + 12.0 && p[2] > 70;
            if sky_blue && !warm {
                row_colors.insert([p[0], p[1], p[2]]);
            }
        }
    }
    Bands {
        sky: sky_hit as f32 / sky_n.max(1) as f32,
        ground: ground_hit as f32 / ground_n.max(1) as f32,
        effect: effect as f32 / px.len().max(1) as f32,
        mean_luma: luma_sum / px.len().max(1) as f32,
        zenith_far: zenith_far as f32 / zenith_n.max(1) as f32,
        zenith_row_colors: row_colors.len() as u32,
    }
}

/// Saves `pixels` as a PNG next to `path`'s parent (plain png-encode, like the sample).
fn save_png(path: &std::path::Path, pixels: &[u8], width: u32, height: u32) {
    use std::io::BufWriter;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("png create");
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
    writer.finish().expect("png finish");
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn window_mode_frame_shows_ground_sky_and_effect() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 640×360: the full runtime pipeline (prepass, clusters, shadows, forward, IBL,
    // post) runs on lavapipe in CI — 16× fewer pixels than the window default keeps
    // the software rasteriser inside the test budget.
    let (width, height) = (640u32, 360u32);
    let mut app = window_app();

    // Baseline (no effect fired yet: the director fires the first effect at t = 0.6 s).
    for _ in 0..12 {
        app.update();
    }
    let before = capture(&mut app, width, height);
    let bands_before = measure(&before, width);

    // Mid-effect: the director fires muzzle flash on frame 39 (sprites live
    // 0.05–0.08 s). Capture on frame 40, one step later.
    for _ in 12..40 {
        app.update();
    }
    let after = capture(&mut app, width, height);
    let bands = measure(&after, width);

    if let Ok(dir) = std::env::var("REGIA_EVIDENCE_DIR") {
        let path = std::path::Path::new(&dir).join("runtime_window.png");
        save_png(&path, &after, width, height);
        println!("runtime window evidence: {}", path.display());
    }
    println!("before effect: {bands_before:?}");
    println!("mid  effect  : {bands:?}");

    // Ground: the lit green ground fills most of the lower half.
    assert!(
        bands.ground > 0.5,
        "ground missing: only {:.1}% of the lower half is green (round-3 defect)",
        bands.ground * 100.0
    );
    // Sky: the upper band is blue.
    assert!(
        bands.sky > 0.5,
        "sky missing: only {:.1}% of the upper band is blue",
        bands.sky * 100.0
    );
    // Flat sky outside the effect. The round-5 window PNG fails this: zenith
    // band ~68% away from the modal sky, ~500 colours on the top-band row
    // (lingering weather mist over a hidden-window frame 236). A flat emissive
    // sky is one colour there.
    assert!(
        bands.zenith_row_colors >= 1 && bands.zenith_row_colors <= 2,
        "top-band row outside the effect has {} sky colors (want ~1; round-5 lobe)",
        bands.zenith_row_colors
    );
    assert!(
        bands.zenith_far < 0.08,
        "zenith band {:.1}% far from the modal sky (round-5 lobe was ~65%)",
        bands.zenith_far * 100.0
    );
    // Effect: present (the mid-effect frame gained warm or white-hot pixels the baseline
    // did not have), but not flooding the frame. The camera does not move between the
    // two captures, so a per-pixel comparison isolates the effect. (The old check, warm
    // pixels > 3x the baseline's, relied on the broken tonemap fit crushing the sunlit
    // red crate to near-black; under the corrected transform the crate reads warm in
    // both frames, and the cream flash core prints white-hot, not orange.)
    let added = effect_added(&before, &after);
    assert!(
        added > 2e-4,
        "effect pixels missing mid-effect ({:.3}% of the frame changed to warm/white-hot; warm {:.3}% vs baseline {:.3}%)",
        added * 100.0,
        bands.effect * 100.0,
        bands_before.effect * 100.0
    );
    assert!(
        bands.effect < 0.4,
        "effect floods the frame ({:.1}% >= 40%) — camera/projection defect",
        bands.effect * 100.0
    );
    // Whole-frame sanity: neither near-black (missing scene) nor blown out.
    assert!(
        bands.mean_luma > 0.02 && bands.mean_luma < 0.8,
        "frame luminance out of range: {:.3}",
        bands.mean_luma
    );

    // Frame 236 is the `--exit-after-frames 240` screenshot. A hidden window
    // steps ~100 ms, which used to put that frame inside lingering weather
    // mist (zenith_far ~65%, hundreds of sky colours). The director is framed
    // in slots so this frame is sparks, and the sky outside that effect is flat.
    app.world_mut()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    while app.world().resource::<Time>().frame_count() < 236 {
        app.update();
    }
    let shot = capture(&mut app, width, height);
    let late = measure(&shot, width);
    assert!(
        late.ground > 0.5 && late.sky > 0.5,
        "frame 236 lost the scene: ground {:.1}% sky {:.1}%",
        late.ground * 100.0,
        late.sky * 100.0
    );
    assert!(
        late.zenith_row_colors >= 1 && late.zenith_row_colors <= 2,
        "frame 236 top row has {} sky colors (weather mist was hundreds)",
        late.zenith_row_colors
    );
    assert!(
        late.zenith_far < 0.08,
        "frame 236 zenith {:.1}% far from the modal sky",
        late.zenith_far * 100.0
    );
    assert!(
        late.effect > 2e-4 && late.effect < 0.4,
        "frame 236 effect coverage {:.3}%",
        late.effect * 100.0
    );
    assert!(
        late.mean_luma > 0.02 && late.mean_luma < 0.8,
        "frame 236 luminance out of range: {:.3}",
        late.mean_luma
    );
}

/// Fires one synthetic drag through the same path the platform input systems use
/// (written in `PreUpdate`, read by `Update` systems via `iter_current_update_messages`).
#[derive(Resource, Default)]
struct FireDrag(bool);

fn fire_drag(
    mut fire: ResMut<FireDrag>,
    mut buttons: MessageWriter<MouseButtonInput>,
    mut motion: MessageWriter<MouseMotion>,
) {
    if !fire.0 {
        return;
    }
    fire.0 = false;
    buttons.write(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state: ButtonState::Pressed,
        timestamp: regia::platform::InputTimestamp::ZERO,
    });
    for _ in 0..10 {
        motion.write(MouseMotion {
            delta: (12.0, 0.0),
            timestamp: regia::platform::InputTimestamp::ZERO,
        });
    }
}

/// The root-cause probe for the round-2 defect: dragging must move the camera the
/// renderer extracts. `orbit_control` writes `Transform`; only the scene plugin's
/// `Transform` → `GlobalTransform` propagation (PostUpdate, before `Render`
/// extraction) carries that into `ExtractedFrame`. With the propagation missing the
/// camera stays frozen at its spawn pose — exactly what put it inside the effects
/// and left the scene edge-on in round 2 (the pre-filled `GlobalTransform`s at spawn
/// alone cannot track input).
#[test]
fn window_mode_camera_tracks_orbit_input() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = window_app();
    app.init_resource::<FireDrag>().add_systems(PreUpdate, fire_drag);
    for _ in 0..5 {
        app.update();
    }
    let initial = app
        .world()
        .resource::<ExtractedFrame>()
        .views
        .first()
        .expect("camera view extracted")
        .camera
        .eye;

    // Press and drag: 10 × 12 px of leftward motion orbits the yaw.
    app.world_mut().resource_mut::<FireDrag>().0 = true;
    for _ in 0..2 {
        app.update();
    }
    let moved = app
        .world()
        .resource::<ExtractedFrame>()
        .views
        .first()
        .expect("camera view extracted after drag")
        .camera
        .eye;
    let drift = (moved[0] - initial[0])
        .hypot(moved[1] - initial[1])
        .hypot(moved[2] - initial[2]);
    assert!(
        drift > 0.1,
        "dragging did not move the extracted camera (drift {drift:.3}) — \
         Transform → GlobalTransform propagation is not wired into the window app"
    );
}
