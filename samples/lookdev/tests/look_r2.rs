// SPDX-License-Identifier: MIT-0
//! Look-director round 2 acceptance on lavapipe (`lookdev::look_r2`): the three
//! measurable failures of the UE parity plates, on the runtime frame path.
//!
//! - Mars dusk: the far mesas keep their light/shadow contrast against the near ground
//!   (the plate was a white-pink fog wall).
//! - Tunnel interior: the floor in the truss shadows is not crushed, the sunlit floor
//!   and the exterior are not clipped.
//! - Cockpit: Jupiter from Europa is as large as on the UE plate (≈ 0.28 of the frame
//!   height through a 90° horizontal lens).

use lookdev::look_r2::{CameraOverride, R2Options, R2Planets, R2Shot, luma, region_percentile, render_shot};
use regia_render::RenderDevice;

fn options() -> R2Options {
    R2Options {
        width: 320,
        height: 180,
        frames: 10,
        terrain_resolution: 513,
        timing: false,
    }
}

fn render(device: &RenderDevice, planets: &mut R2Planets, shot: R2Shot) -> Vec<u8> {
    let (pixels, _) = render_shot(device, planets, shot, options(), CameraOverride::default())
        .unwrap_or_else(|err| panic!("{}: {err}", shot.name()));
    if std::env::var_os("REGIA_WRITE_EVIDENCE").is_some() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/look_r2_tests");
        let _ = std::fs::create_dir_all(&dir);
        let _ = lookdev::default_look::write_png(
            &dir.join(format!("{}.png", shot.name())),
            options().width,
            options().height,
            &pixels,
        );
    }
    pixels
}

/// Spread of luma (p90 − p10) inside a rect given in frame fractions.
fn spread(pixels: &[u8], rect: [f32; 4]) -> f32 {
    let o = options();
    region_percentile(pixels, o.width, o.height, rect, 90.0)
        - region_percentile(pixels, o.width, o.height, rect, 10.0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn dusk_far_mesas_keep_contrast_against_the_near_ground() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    let pixels = render(&device, &mut planets, R2Shot::DuskVista);
    // The mesas fill the band under the horizon; the near ground below them carries the
    // long shadow of the low sun. Round 4 turned the camera to put the sun ahead and to
    // the right (as on the plate), so the mesa band starts right of the open sky.
    let far = spread(&pixels, [0.15, 0.32, 1.0, 0.45]);
    let near = spread(&pixels, [0.0, 0.40, 1.0, 0.75]);
    let ratio = far / near.max(1.0);
    eprintln!("dusk: far mesa spread {far:.1}, near ground spread {near:.1}, ratio {ratio:.2}");
    assert!(
        far >= 45.0,
        "far mesas are a flat fog wall: p90-p10 luma {far:.1} (UE plate keeps lit and shadowed faces apart)"
    );
    assert!(
        near >= 20.0,
        "the near ground is a flat sheet of fog: p90-p10 luma {near:.1} (UE: dunes and long shadows)"
    );
    assert!(
        (0.4..=2.5).contains(&ratio),
        "near ground and far mesas do not read as one lit scene: far/near contrast {ratio:.2}"
    );
}

#[test]
fn tunnel_floor_in_shadow_is_not_crushed_and_the_exterior_is_not_blown() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    let pixels = render(&device, &mut planets, R2Shot::TunnelInterior);
    let o = options();
    // The floor under the roof, in the roof's shadow, left of the walkway.
    let shadow_floor = region_percentile(&pixels, o.width, o.height, [0.05, 0.80, 0.40, 1.0], 50.0);
    // The bright exit: the share of clipped pixels around the open end.
    let exit = [0.30, 0.15, 0.70, 0.55];
    let (x0, x1) = (
        (exit[0] * o.width as f32) as u32,
        (exit[2] * o.width as f32) as u32,
    );
    let (y0, y1) = (
        (exit[1] * o.height as f32) as u32,
        (exit[3] * o.height as f32) as u32,
    );
    let mut clipped = 0u32;
    let mut total = 0u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * o.width + x) * 4) as usize;
            let px = &pixels[i..i + 4];
            total += 1;
            if px[0] >= 250 && px[1] >= 250 && px[2] >= 250 {
                clipped += 1;
            }
        }
    }
    let clipped_pct = 100.0 * clipped as f32 / total.max(1) as f32;
    eprintln!("tunnel: shadowed floor median luma {shadow_floor:.1}, exit clipped {clipped_pct:.1} %");
    assert!(
        shadow_floor >= 45.0,
        "the shadowed floor is crushed: median luma {shadow_floor:.1} (UE: bounce from the dusty floor)"
    );
    assert!(
        clipped_pct <= 8.0,
        "the exterior is blown: {clipped_pct:.1} % of the exit is pure white"
    );
}

/// Bounding extent (max of width, height) of the largest bright blob above the horizon
/// (the ice meets the sky near 48 % of the frame height).
fn largest_sky_blob(pixels: &[u8], width: u32, height: u32) -> u32 {
    let sky_rows = height * 42 / 100;
    let lit = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        luma(&pixels[i..i + 4]) > 18.0
    };
    let mut seen = vec![false; (width * sky_rows) as usize];
    let mut best = 0;
    for y in 0..sky_rows {
        for x in 0..width {
            if seen[(y * width + x) as usize] || !lit(x, y) {
                continue;
            }
            let (mut minx, mut maxx, mut miny, mut maxy) = (x, x, y, y);
            let mut stack = vec![(x, y)];
            seen[(y * width + x) as usize] = true;
            while let Some((cx, cy)) = stack.pop() {
                minx = minx.min(cx);
                maxx = maxx.max(cx);
                miny = miny.min(cy);
                maxy = maxy.max(cy);
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let (nx, ny) = (cx as i32 + dx, cy as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= width as i32 || ny >= sky_rows as i32 {
                        continue;
                    }
                    let (nx, ny) = (nx as u32, ny as u32);
                    let k = (ny * width + nx) as usize;
                    if !seen[k] && lit(nx, ny) {
                        seen[k] = true;
                        stack.push((nx, ny));
                    }
                }
            }
            best = best.max((maxx - minx + 1).max(maxy - miny + 1));
        }
    }
    best
}

#[test]
fn jupiter_from_europa_is_as_large_as_the_ue_plate() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    let pixels = render(&device, &mut planets, R2Shot::Cockpit);
    let o = options();
    let diameter = largest_sky_blob(&pixels, o.width, o.height);
    let fraction = diameter as f32 / o.height as f32;
    eprintln!("cockpit: Jupiter {diameter} px = {fraction:.3} of the frame height");
    // UE v13 plate: ≈ 300 of 1080 rows (0.278) through the 90° horizontal camera, at the
    // same screen position. Within 10 %.
    assert!(
        (0.25..=0.305).contains(&fraction),
        "Jupiter spans {fraction:.3} of the frame height; the UE plate shows ≈ 0.278"
    );
}

/// Mean hue angle (degrees) of the saturated pixels (HSV saturation ≥ 0.35, value ≥ 0.12) in a
/// rect given in frame fractions; a circular mean.
fn saturated_hue(pixels: &[u8], width: u32, height: u32, rect: [f32; 4]) -> (f32, usize) {
    let (x0, x1) = ((rect[0] * width as f32) as u32, (rect[2] * width as f32) as u32);
    let (y0, y1) = ((rect[1] * height as f32) as u32, (rect[3] * height as f32) as u32);
    let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let i = ((y * width + x) * 4) as usize;
            let (r, g, b) = (
                f32::from(pixels[i]) / 255.0,
                f32::from(pixels[i + 1]) / 255.0,
                f32::from(pixels[i + 2]) / 255.0,
            );
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            if mx < 0.12 || mx <= 0.0 || (mx - mn) / mx < 0.35 {
                continue;
            }
            let d = mx - mn;
            let h = if mx == r {
                ((g - b) / d).rem_euclid(6.0)
            } else if mx == g {
                (b - r) / d + 2.0
            } else {
                (r - g) / d + 4.0
            } * 60.0;
            let a = f64::from(h).to_radians();
            sx += a.cos();
            sy += a.sin();
            n += 1;
        }
    }
    ((sy.atan2(sx).to_degrees().rem_euclid(360.0)) as f32, n)
}

#[test]
fn switchbacks_mesas_print_the_ue_red_brown_not_olive() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    let pixels = render(&device, &mut planets, R2Shot::SwitchbacksWide);
    let o = options();
    // The big mesa on the right of the frame.
    let (hue, n) = saturated_hue(&pixels, o.width, o.height, [0.55, 0.08, 0.98, 0.33]);
    eprintln!("switchbacks_wide: mesa hue {hue:.1}° over {n} saturated pixels");
    assert!(n > 200, "the mesa region has saturated pixels ({n})");
    // UE v13 plates, same mask: switchbacks_wide 23.0°, switchbacks_mid 25.0°, dusk 22.3°,
    // tunnel 22.5° (circular mean of saturated pixels). Warm red-brown, not ochre/olive (≥ 29°).
    assert!(
        (20.0..=26.0).contains(&hue),
        "mesas print at hue {hue:.1}°; the UE plates are 22–25° (red-brown)"
    );
}
