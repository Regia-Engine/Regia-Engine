// SPDX-License-Identifier: MPL-2.0
//! Lavapipe goldens for the physical atmosphere, froxel fog and volumetric clouds.

use regia_render::{
    AtmosphereParameters, CloudPattern, CloudSettings, RenderDevice, SkyView, TimeOfDay, render_analytic_sky,
    render_cloud_coverage, render_cloud_ground_shadow, render_fog_shaft, render_local_fog, render_sky_view,
    render_time_of_day_sky, sun_position,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, compare_rgba8_to_png_on, golden_path, ssim_rgba8,
};

/// The process's one shared device: a device per parallel test hung the NVIDIA driver under load.
fn device() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip atmosphere goldens: {err}");
            None
        }
    }
}

fn luma_at(pixels: &[u8], width: u32, x: u32, y: u32) -> f32 {
    let i = ((y * width + x) * 4) as usize;
    0.2126 * pixels[i] as f32 + 0.7152 * pixels[i + 1] as f32 + 0.0722 * pixels[i + 2] as f32
}

fn mean_luma(pixels: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for px in pixels.as_chunks::<4>().0 {
        sum += 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
        n += 1.0;
    }
    sum / n.max(1.0)
}

fn max_luma_window(pixels: &[u8], width: u32, height: u32, cx: u32, cy: u32, radius: u32) -> f32 {
    let mut best = 0.0f32;
    let x0 = cx.saturating_sub(radius);
    let y0 = cy.saturating_sub(radius);
    let x1 = (cx + radius).min(width - 1);
    let y1 = (cy + radius).min(height - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            best = best.max(luma_at(pixels, width, x, y));
        }
    }
    best
}

fn channel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

#[test]
fn sky_four_times_of_day_have_the_right_hue() {
    let Some(device) = device() else { return };
    let size = (128, 96);
    let fov = 12.0_f32.to_radians();
    let noon = render_time_of_day_sky(&device, &TimeOfDay::noon(), size, fov, true).expect("noon");
    // Dawn and dusk share one fixed panorama (azimuth across, north at the left edge), so
    // the glow has to move from the east column to the west column.
    let pano = |time: TimeOfDay| SkyView {
        time,
        planet: AtmosphereParameters::earth(),
        body: None,
        size: (180, 60),
        fov_y: 1.0,
        look_dir: [0.0, 0.0, 1.0],
        panorama: true,
    };
    let dawn = render_sky_view(&device, &pano(TimeOfDay::dawn())).expect("dawn");
    let dusk = render_sky_view(&device, &pano(TimeOfDay::dusk())).expect("dusk");
    let dawn_disk = render_time_of_day_sky(&device, &TimeOfDay::dawn(), size, fov, true).expect("dawn disk");
    // Wide view of the horizon. A zenith crop would hide the moonlit gradient.
    let night = render_time_of_day_sky(
        &device,
        &TimeOfDay::night(),
        (160, 100),
        70.0_f32.to_radians(),
        false,
    )
    .expect("night");

    let noon_disk = max_luma_window(&noon.pixels, noon.width, noon.height, 64, 48, 4);
    assert!(noon_disk > 240.0, "noon sun disk luma {noon_disk}");
    let corner = channel(&noon.pixels, noon.width, 4, 4);
    assert!(
        corner[2] > corner[0] + 15,
        "noon sky should be blue, got {corner:?}"
    );
    assert!(
        luma_at(&noon.pixels, noon.width, 4, 4) < 210.0,
        "noon corner is the disk"
    );

    assert_warm_near_disk(&dawn_disk.pixels, dawn_disk.width, dawn_disk.height, "dawn");
    assert_glow_on_sun_side(&dawn.pixels, dawn.width, dawn.height, &TimeOfDay::dawn(), "dawn");
    assert_glow_on_sun_side(&dusk.pixels, dusk.width, dusk.height, &TimeOfDay::dusk(), "dusk");
    // The two frames are mirror-different: a dawn golden blessed as dusk would not match.
    let delta = mean_abs_delta(&dawn.pixels, &dusk.pixels);
    assert!(
        delta > 1.5,
        "dawn and dusk frames are nearly the same image, mean |Δ| {delta}"
    );
    // A dusk frame checked against a dawn golden (a swapped bless) must fail the threshold.
    let ssim = ssim_rgba8(&dawn.pixels, &dusk.pixels, dawn.width, dawn.height).expect("ssim");
    assert!(
        ssim < DEFAULT_SSIM_THRESHOLD,
        "dawn and dusk panoramas would pass each other's golden (SSIM {ssim})"
    );

    let night_mean = mean_luma(&night.pixels);
    assert!(
        night_mean > 8.0 && night_mean < 80.0,
        "night mean {night_mean} should be a dark sky, not black and not daylight"
    );
    assert_night_horizon_and_stars(&night.pixels, night.width, night.height);

    for (name, image) in [
        ("sky_noon", &noon),
        ("sky_dawn", &dawn),
        ("sky_dusk", &dusk),
        ("sky_night", &night),
    ] {
        // Lavapipe-blessed (ADR-025): a hardware GPU is held to the sanity floor (ADR-270), so
        // the RTX 3080 Ti / 5080 sky_noon at SSIM 0.978 is not a failure; the hue, disk, glow and
        // night assertions above still run on every adapter.
        compare_rgba8_to_png_on(
            &device.adapter.get_info(),
            &image.pixels,
            image.width,
            image.height,
            &golden_path(name),
            DEFAULT_SSIM_THRESHOLD,
        )
        .unwrap_or_else(|err| panic!("{name}: {err}"));
    }
}

fn mean_abs_delta(a: &[u8], b: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        sum += (f32::from(*x) - f32::from(*y)).abs();
    }
    sum / a.len().max(1) as f32
}

/// Mean (R, B, luma) of the panorama's band just above the horizon (elevation 0–15°) over
/// azimuths `az0..az1` degrees (clockwise from north).
fn horizon_band(pixels: &[u8], width: u32, height: u32, az0: f32, az1: f32) -> (f32, f32, f32) {
    let x0 = (az0 / 360.0 * width as f32) as u32;
    let x1 = (az1 / 360.0 * width as f32) as u32;
    // Row y maps to elevation 90 - 100·(y + 0.5)/height.
    let y0 = (height as f32 * 0.75) as u32;
    let y1 = (height as f32 * 0.9) as u32;
    let mut acc = (0.0f32, 0.0f32, 0.0f32);
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in x0..x1.min(width) {
            let rgb = channel(pixels, width, x, y);
            // Skip the solar disc and its bloom-free white core.
            if luma_at(pixels, width, x, y) > 245.0 {
                continue;
            }
            acc.0 += f32::from(rgb[0]);
            acc.1 += f32::from(rgb[2]);
            acc.2 += luma_at(pixels, width, x, y);
            n += 1.0;
        }
    }
    let n = n.max(1.0);
    (acc.0 / n, acc.1 / n, acc.2 / n)
}

/// The horizon toward the sun's azimuth must be brighter and redder than the opposite one.
fn assert_glow_on_sun_side(pixels: &[u8], width: u32, height: u32, time: &TimeOfDay, label: &str) {
    let sun = sun_position(time).direction;
    let az = sun[0].atan2(sun[2]).to_degrees().rem_euclid(360.0);
    let opposite = (az + 180.0).rem_euclid(360.0);
    let near = horizon_band(
        pixels,
        width,
        height,
        (az - 25.0).max(0.0),
        (az + 25.0).min(360.0),
    );
    let far = horizon_band(
        pixels,
        width,
        height,
        (opposite - 25.0).max(0.0),
        (opposite + 25.0).min(360.0),
    );
    assert!(
        near.2 > far.2 + 8.0,
        "{label}: the sun side should be brighter, near {near:?} far {far:?}"
    );
    let warmth = |t: (f32, f32, f32)| t.0 / t.1.max(1.0);
    assert!(
        warmth(near) > warmth(far) + 0.03,
        "{label}: the sun side should be warmer, near {near:?} far {far:?}"
    );
}

/// Horizon band is brighter and bluer than the upper sky, and a few pixels
/// stick out of their neighbourhood (stars). A clear-to-black, a flat fill,
/// and a luma<80 gate all fail this.
fn assert_night_horizon_and_stars(pixels: &[u8], width: u32, height: u32) {
    let upper = band_rgb(pixels, width, height, height / 4, height * 2 / 5);
    let horizon = band_rgb(pixels, width, height, height * 58 / 100, height * 72 / 100);
    assert!(
        horizon.3 > upper.3 * 1.35 + 4.0,
        "moonlit horizon luma {} should rise above the upper sky {}",
        horizon.3,
        upper.3
    );
    assert!(
        horizon.2 > horizon.0 + 6.0,
        "horizon should be blue, rgb ({}, {}, {})",
        horizon.0,
        horizon.1,
        horizon.2
    );
    let stars = star_pixels(pixels, width, height);
    let fraction = stars as f32 / (width * height) as f32;
    assert!(
        (12..700).contains(&stars) && fraction < 0.08,
        "stars should be sparse points, count {stars} fraction {fraction}"
    );
}

fn band_rgb(pixels: &[u8], width: u32, height: u32, y0: u32, y1: u32) -> (f32, f32, f32, f32) {
    let mut acc = [0.0f32; 4];
    let mut n = 0.0f32;
    for y in y0..y1.min(height) {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            acc[0] += pixels[i] as f32;
            acc[1] += pixels[i + 1] as f32;
            acc[2] += pixels[i + 2] as f32;
            acc[3] += luma_at(pixels, width, x, y);
            n += 1.0;
        }
    }
    let n = n.max(1.0);
    (acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n)
}

fn star_pixels(pixels: &[u8], width: u32, height: u32) -> u32 {
    let mut count = 0u32;
    if width < 3 || height < 3 {
        return 0;
    }
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let here = luma_at(pixels, width, x, y);
            if here < 140.0 {
                continue;
            }
            let mut local = 0.0f32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    local += luma_at(pixels, width, (x as i32 + dx) as u32, (y as i32 + dy) as u32);
                }
            }
            if here > local / 8.0 + 40.0 {
                count += 1;
            }
        }
    }
    count
}

fn assert_warm_near_disk(pixels: &[u8], width: u32, height: u32, label: &str) {
    let mut best = (0u32, 0u32, 0.0f32);
    for y in 0..height {
        for x in 0..width {
            let yuma = luma_at(pixels, width, x, y);
            if yuma > best.2 {
                best = (x, y, yuma);
            }
        }
    }
    assert!(best.2 > 240.0, "{label} disk luma {}", best.2);
    let mut warm = false;
    for y in best.1.saturating_sub(8)..=(best.1 + 8).min(height - 1) {
        for x in best.0.saturating_sub(8)..=(best.0 + 8).min(width - 1) {
            let rgb = channel(pixels, width, x, y);
            let yuma = luma_at(pixels, width, x, y);
            if yuma < 220.0 && rgb[0] > rgb[2] + 8 {
                warm = true;
            }
        }
    }
    assert!(warm, "{label} sky next to the disk is not warm");
}

#[test]
fn analytic_sky_has_no_sun_disk() {
    let Some(device) = device() else { return };
    let image = render_analytic_sky(&device, 64, 48, 50.0_f32.to_radians()).expect("analytic");
    let white = image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|px| px[0] > 250 && px[1] > 250 && px[2] > 250);
    assert!(!white, "analytic sky drew a sun disk");
    assert!(mean_luma(&image.pixels) > 20.0, "analytic sky is black");
}

#[test]
fn fog_shaft_is_brighter_than_its_shadow_and_survives_history() {
    let Some(device) = device() else { return };
    let images = render_fog_shaft(&device).expect("fog shaft");
    let profile = shaft_profile(&images.first.pixels, images.first.width, images.first.height);
    assert!(
        profile.peak_x > images.first.width * 3 / 10 && profile.peak_x < images.first.width * 7 / 10,
        "shaft peak x {} should sit in the gap, not a half-plane",
        profile.peak_x
    );
    assert!(
        profile.peak > profile.left + 18.0 && profile.peak > profile.right + 18.0,
        "peak {} shoulders {} {}",
        profile.peak,
        profile.left,
        profile.right
    );
    let contrast = profile.peak - profile.left.min(profile.right);
    assert!(
        (profile.left - profile.right).abs() < contrast * 0.55,
        "shoulders should both be dark, left {} right {} peak {}",
        profile.left,
        profile.right,
        profile.peak
    );
    assert!(
        (0.04..0.28).contains(&profile.width_frac),
        "shaft width fraction {} is a half-plane or a hairline",
        profile.width_frac
    );
    assert!(
        profile.rise_px >= 3,
        "the shaft edge rose in {} px; a hard 0/255 bar fails",
        profile.rise_px
    );
    assert!(profile.levels >= 3, "transect has {} luma levels", profile.levels);
    assert!(
        profile.top < profile.peak * 0.75 && profile.bottom < profile.peak * 0.75,
        "shaft fills the frame vertically, top {} bottom {} peak {}",
        profile.top,
        profile.bottom,
        profile.peak
    );
    let later = shaft_profile(&images.second.pixels, images.second.width, images.second.height);
    assert!(
        later.peak > profile.peak * 0.45 && later.peak > later.left + 10.0 && later.peak > later.right + 10.0,
        "history dropped the shaft: frame2 peak {} shoulders {} {} frame1 {}",
        later.peak,
        later.left,
        later.right,
        profile.peak
    );
    compare_rgba8_to_png(
        &images.first.pixels,
        images.first.width,
        images.first.height,
        &golden_path("fog_shaft"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .unwrap_or_else(|err| panic!("fog_shaft: {err}"));
}

struct ShaftProfile {
    peak: f32,
    peak_x: u32,
    left: f32,
    right: f32,
    width_frac: f32,
    rise_px: u32,
    levels: u32,
    top: f32,
    bottom: f32,
}

fn shaft_profile(pixels: &[u8], width: u32, height: u32) -> ShaftProfile {
    let y0 = height / 3;
    let y1 = (height * 2 / 3).max(y0 + 1);
    let mut cols = vec![0.0f32; width as usize];
    for (x, col) in cols.iter_mut().enumerate() {
        let mut sum = 0.0f32;
        let mut n = 0.0f32;
        for y in y0..y1 {
            sum += luma_at(pixels, width, x as u32, y);
            n += 1.0;
        }
        *col = sum / n.max(1.0);
    }
    let peak_x = cols
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(index, _)| index)
        .unwrap_or(0);
    let peak = cols[peak_x];
    let mean = |range: std::ops::Range<usize>| {
        let slice = &cols[range.start.min(cols.len())..range.end.min(cols.len())];
        if slice.is_empty() {
            0.0
        } else {
            slice.iter().sum::<f32>() / slice.len() as f32
        }
    };
    let left = mean(width as usize / 5..width as usize * 2 / 5);
    let right = mean(width as usize * 3 / 5..width as usize * 4 / 5);
    let shoulder = (left + right) * 0.5;
    let mid = shoulder + (peak - shoulder) * 0.5;
    let mut x0 = peak_x;
    let mut x1 = peak_x;
    while x0 > 0 && cols[x0] > mid {
        x0 -= 1;
    }
    while x1 + 1 < cols.len() && cols[x1] > mid {
        x1 += 1;
    }
    let width_frac = (x1 - x0) as f32 / width.max(1) as f32;
    let lo = left + 0.2 * (peak - left);
    let hi = left + 0.8 * (peak - left);
    let mut rise_lo = peak_x;
    let mut rise_hi = peak_x;
    for x in (0..=peak_x).rev() {
        if cols[x] <= hi {
            rise_hi = x;
            break;
        }
    }
    for x in (0..=peak_x).rev() {
        if cols[x] <= lo {
            rise_lo = x;
            break;
        }
    }
    let rise_px = rise_hi.saturating_sub(rise_lo);
    let mut seen = [false; 8];
    let span = (peak - shoulder).abs().max(1.0);
    for col in &cols {
        let bucket = (((col - shoulder) / span).clamp(0.0, 0.999) * 8.0) as usize;
        seen[bucket.min(7)] = true;
    }
    let levels = seen.iter().filter(|on| **on).count() as u32;
    let row_mean = |y0: u32, y1: u32| {
        let x_lo = peak_x.saturating_sub(2);
        let x_hi = (peak_x + 3).min(width as usize);
        let mut sum = 0.0f32;
        let mut n = 0.0f32;
        for y in y0..y1 {
            for x in x_lo..x_hi {
                sum += luma_at(pixels, width, x as u32, y);
                n += 1.0;
            }
        }
        sum / n.max(1.0)
    };
    let edge = (height / 10).max(1);
    ShaftProfile {
        peak,
        peak_x: peak_x as u32,
        left,
        right,
        width_frac,
        rise_px: rise_px as u32,
        levels,
        top: row_mean(0, edge),
        bottom: row_mean(height.saturating_sub(edge), height),
    }
}

#[test]
fn local_fog_light_is_brighter_at_the_centre() {
    let Some(device) = device() else { return };
    let image = render_local_fog(&device).expect("local fog");
    let mid = luma_at(&image.pixels, image.width, image.width / 2, image.height / 2);
    let corner = luma_at(&image.pixels, image.width, 1, 1);
    assert!(mid > corner + 15.0, "center {mid} vs corner {corner}");
}

#[test]
fn cloud_coverage_matches_the_setting_within_ten_percent() {
    let Some(device) = device() else { return };
    let settings = CloudSettings::coverage_layer(0.5);
    let image = render_cloud_coverage(&device, &settings).expect("clouds");
    let fraction = cloud_fraction(&image.pixels);
    assert!(
        (fraction - 0.5).abs() <= 0.10,
        "coverage fraction {fraction} image {}x{}",
        image.width,
        image.height
    );
    let spread = cloudy_luma_stddev(&image.pixels);
    assert!(
        spread > 6.0,
        "cloudy pixels are one flat value (a weather-mask blit), stddev {spread}"
    );
    let mut wind = settings;
    wind.wind_m_per_s = [8.0, 0.0];
    wind.time_s = 10.0;
    let scrolled = render_cloud_coverage(&device, &wind).expect("wind");
    assert_ne!(image.pixels, scrolled.pixels, "wind did not move the clouds");
    // The coverage knob, not a fixed mask, sets the cloudy fraction: a quarter and three
    // quarters land within ±10 % too, and an empty constant weather map has no cloud at all.
    for want in [0.25_f32, 0.75] {
        let other = render_cloud_coverage(&device, &CloudSettings::coverage_layer(want)).expect("clouds");
        let got = cloud_fraction(&other.pixels);
        assert!((got - want).abs() <= 0.10, "coverage {want} measured {got}");
    }
    let empty = render_cloud_coverage(
        &device,
        &CloudSettings {
            pattern: CloudPattern::Constant,
            ..CloudSettings::coverage_layer(0.0)
        },
    )
    .expect("empty weather");
    assert_eq!(cloud_fraction(&empty.pixels), 0.0, "no weather, no cloud");
    assert!(
        adjacent_contrast(&image.pixels, image.width) > 2.0,
        "the cloud field has no edges"
    );
    compare_rgba8_to_png(
        &image.pixels,
        image.width,
        image.height,
        &golden_path("cloud_coverage"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .unwrap_or_else(|err| panic!("cloud_coverage: {err}"));
}

#[test]
fn cloud_shadow_darkens_the_cloudy_half() {
    let Some(device) = device() else { return };
    let image = render_cloud_ground_shadow(&device, &CloudSettings::split_shadow()).expect("shadow");
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    let mut ln = 0.0f32;
    let mut rn = 0.0f32;
    for y in 0..image.height {
        for x in 0..image.width {
            let yuma = luma_at(&image.pixels, image.width, x, y);
            if x < image.width / 2 {
                left += yuma;
                ln += 1.0;
            } else {
                right += yuma;
                rn += 1.0;
            }
        }
    }
    let left = left / ln;
    let right = right / rn;
    assert!(left > right * 1.3, "clear {left} vs cloudy {right}");
}

fn is_cloud_pixel(px: &[u8; 4]) -> bool {
    let r = i32::from(px[0]);
    let b = i32::from(px[2]);
    r + 8 >= b && r > 90
}

fn cloud_fraction(pixels: &[u8]) -> f32 {
    let texels = pixels.as_chunks::<4>().0;
    if texels.is_empty() {
        return 0.0;
    }
    let cloudy = texels.iter().filter(|px| is_cloud_pixel(px)).count();
    cloudy as f32 / texels.len() as f32
}

fn cloudy_luma_stddev(pixels: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    let mut sum_sq = 0.0f32;
    let mut n = 0.0f32;
    for px in pixels.as_chunks::<4>().0 {
        if !is_cloud_pixel(px) {
            continue;
        }
        let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
        sum += y;
        sum_sq += y * y;
        n += 1.0;
    }
    if n < 2.0 {
        return 0.0;
    }
    let mean = sum / n;
    (sum_sq / n - mean * mean).max(0.0).sqrt()
}

fn adjacent_contrast(pixels: &[u8], width: u32) -> f32 {
    if width < 2 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    let texels = pixels.as_chunks::<4>().0;
    let height = texels.len() as u32 / width;
    for y in 0..height {
        for x in 0..width - 1 {
            let a = luma_at(pixels, width, x, y);
            let b = luma_at(pixels, width, x + 1, y);
            sum += (a - b).abs();
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}
