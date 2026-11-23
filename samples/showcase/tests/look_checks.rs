// SPDX-License-Identifier: MIT-0
//! Showcase look checks (lane showcase-look-checks, ADR-2486): measured reports, not goldens.
//!
//! - `sky_at_4k` (ignored, slow on lavapipe): the physical sky, the star field and the clouds at 1920x1080 and
//!   3840x2160 from the same camera, with the numbers that tell a low-resolution or quantized sky apart: the
//!   longest run of one 8-bit value down a sky column (banding), the star count and peak (star field), and the
//!   high-frequency energy of the cloud deck (detail). `LOOK_CHECK_OUT=<dir>` keeps the frames.
//! - `lite_tier_forest` (ignored, slow on lavapipe): the forest view at Desktop High, Desktop Low (what an
//!   integrated GPU is given) and Mobile Low with the lite forward shader forced, with the frame statistics.
//! - `mobile_low_lite_forest_draws_a_scene` (runs by default): the lowest tier still draws terrain, trees and a
//!   sky, not a flat or black frame.
//!
//! The pure image metrics have their own unit tests below, so a number in the report means what it says.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use common::{JobSpec, run, scene_dir};
use regia_render::{RenderDevice, RenderProfilePreset};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

/// Rec.709 luma of an RGBA8 pixel, 0..=255 scale (on the encoded values, which is what banding is seen on).
fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Banding along one image column: the longest run of identical RGB values between `rows`, and the share of
/// rows that sit in a run at least `flat` long. A smooth gradient quantized to 8 bits without dither shows long
/// runs (each run is one visible band); a dithered one has runs of a few pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Banding {
    longest_run: usize,
    flat_share: f32,
    distinct: usize,
}

fn column_banding(w: u32, h: u32, rgba: &[u8], x: u32, rows: std::ops::Range<u32>, flat: usize) -> Banding {
    let px = |y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    let rows = rows.start.min(h)..rows.end.min(h);
    let mut runs = Vec::new();
    let mut run_len = 0usize;
    let mut prev: Option<[u8; 3]> = None;
    let mut distinct = std::collections::HashSet::new();
    for y in rows.clone() {
        let p = px(y);
        distinct.insert(p);
        if prev == Some(p) {
            run_len += 1;
        } else {
            if run_len > 0 {
                runs.push(run_len);
            }
            run_len = 1;
        }
        prev = Some(p);
    }
    if run_len > 0 {
        runs.push(run_len);
    }
    let total: usize = runs.iter().sum();
    let flat_rows: usize = runs.iter().filter(|&&r| r >= flat).sum();
    Banding {
        longest_run: runs.iter().copied().max().unwrap_or(0),
        flat_share: if total == 0 {
            0.0
        } else {
            flat_rows as f32 / total as f32
        },
        distinct: distinct.len(),
    }
}

/// Point sources: pixels that are a strict 3x3 local luma maximum, at least `above` levels over the median
/// luma of the frame and over the brightest pixel of the ring two pixels out. Returns (count, mean peak luma).
fn star_stats(w: u32, h: u32, rgba: &[u8], above: f32) -> (usize, f32) {
    star_stats_where(w, h, rgba, above, |_, _| true)
}

/// [`star_stats`] over the pixels where `keep(x, y)` holds.
fn star_stats_where(
    w: u32,
    h: u32,
    rgba: &[u8],
    above: f32,
    keep: impl Fn(u32, u32) -> bool,
) -> (usize, f32) {
    let l: Vec<f32> = rgba.as_chunks::<4>().0.iter().map(|p| luma(p)).collect();
    let mut sorted = l.clone();
    sorted.sort_by(f32::total_cmp);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    let (mut count, mut sum) = (0usize, 0.0f32);
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let c = l[(y * w + x) as usize];
            if c < median + above || !keep(x, y) {
                continue;
            }
            // A point source stands out from the ring two pixels away, not just from its neighbours
            // (dither noise and cloud texture make one-level local maxima).
            let mut ring = 0.0f32;
            for d in -2i32..=2 {
                for (dx, dy) in [(d, -2), (d, 2), (-2, d), (2, d)] {
                    let (rx, ry) = (x as i32 + dx, y as i32 + dy);
                    if rx >= 0 && ry >= 0 && rx < w as i32 && ry < h as i32 {
                        ring = ring.max(l[(ry as u32 * w + rx as u32) as usize]);
                    }
                }
            }
            if c < ring + above {
                continue;
            }
            let mut peak = true;
            for (dx, dy) in [
                (-1i32, -1i32),
                (0, -1),
                (1, -1),
                (-1, 0),
                (1, 0),
                (-1, 1),
                (0, 1),
                (1, 1),
            ] {
                let n = l[((y as i32 + dy) as u32 * w + (x as i32 + dx) as u32) as usize];
                if n >= c {
                    peak = false;
                    break;
                }
            }
            if peak {
                count += 1;
                sum += c;
            }
        }
    }
    (count, if count == 0 { 0.0 } else { sum / count as f32 })
}

/// High-frequency energy inside a rectangle: the mean absolute difference between luma and its 3x3 box blur,
/// in 8-bit levels. Detail at the image's own pixel scale; an upscaled low-resolution image scores near 0.
fn high_frequency(w: u32, h: u32, rgba: &[u8], rect: (u32, u32, u32, u32)) -> f32 {
    let l = |x: u32, y: u32| luma(&rgba[((y * w + x) * 4) as usize..]);
    let (x0, y0, x1, y1) = (rect.0.max(1), rect.1.max(1), rect.2.min(w - 1), rect.3.min(h - 1));
    let (mut sum, mut n) = (0.0f64, 0u64);
    for y in y0..y1 {
        for x in x0..x1 {
            let mut blur = 0.0;
            for dy in 0..3 {
                for dx in 0..3 {
                    blur += l(x + dx - 1, y + dy - 1);
                }
            }
            sum += f64::from((l(x, y) - blur / 9.0).abs());
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { (sum / n as f64) as f32 }
}

/// Mean and standard deviation of luma over a band of rows (fractions of the height).
fn band_stats(w: u32, h: u32, rgba: &[u8], from: f32, to: f32) -> (f32, f32) {
    let (y0, y1) = ((h as f32 * from) as u32, (h as f32 * to) as u32);
    let values: Vec<f64> = (y0..y1)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| f64::from(luma(&rgba[((y * w + x) * 4) as usize..])))
        .collect();
    let n = values.len().max(1) as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
    (mean as f32, var.sqrt() as f32)
}

/// A copy of the example scene (symlinks, the source stays read-only) whose `sky.ron` is `sky`.
#[cfg(unix)]
fn sky_variant(tag: &str, sky: &str) -> PathBuf {
    use std::os::unix::fs::symlink;
    let src = scene_dir();
    let dir = unique_temp_dir(tag);
    for entry in std::fs::read_dir(&src).expect("scene dir") {
        let entry = entry.expect("entry");
        if entry.file_name() == "sky.ron" {
            continue;
        }
        symlink(entry.path(), dir.join(entry.file_name())).expect("link");
    }
    std::fs::write(dir.join("sky.ron"), sky).expect("sky.ron");
    dir
}

/// `sky.ron` text: the example's Earth atmosphere at `hour`, `weather` preset and cloud coverage, no changes.
fn sky_ron(hour: f32, weather: &str, coverage: f32) -> String {
    format!(
        "(version: 1, atmosphere: (preset: Earth, time_of_day: Some((hour: {hour:.2}, day_of_year: 80, latitude_deg: 46.0)), \
         sun_illuminance_lux: None, cloud_coverage: {coverage:.2}, cloud_type: 0.0, sky_body: None, moons: [], \
         haze: (density: 0.0, scale_height_m: 200.0, base_height_m: 0.0), aerosol_turbidity: 0.1), \
         look: (exposure_bias_ev: 0.0, min_ev100: None, max_ev100: None, meter: None, contrast: None, saturation: None, \
         grade: None, lut: None, local_exposure: None, baked_gi: None), weather: Preset(\"{weather}\"), changes: [])\n"
    )
}

fn out_dir() -> Option<PathBuf> {
    std::env::var_os("LOOK_CHECK_OUT").map(PathBuf::from)
}

fn keep_dir(name: &str) -> Option<PathBuf> {
    out_dir().map(|d| d.join(name))
}

/// Renders one held frame and returns (width, height, rgba).
fn shot(
    scene: &Path,
    size: (u32, u32),
    camera: ([f32; 3], [f32; 3]),
    extra_calls: Vec<(String, serde_json::Value)>,
    keep: &str,
    warmup: u64,
) -> (u32, u32, Vec<u8>) {
    let spec = JobSpec {
        width: size.0,
        height: size.1,
        frames: 1,
        warmup_frames: warmup,
        plain: false,
        scatter_lod: 1,
        scene: Some(scene.to_path_buf()),
        camera: Some(camera),
        keep: keep_dir(keep),
        calls: extra_calls,
        profile: Some(RenderProfilePreset::DesktopHigh),
        ..JobSpec::default()
    };
    run(0, &spec).first_frame
}

/// Look up from high ground, so the frame above the lower fifth is sky: away from the sun for the gradient.
const SKY_CAMERA: ([f32; 3], [f32; 3]) = ([0.0, 420.0, 0.0], [0.0, 560.0, -1000.0]);

#[test]
#[ignore = "look check: 1080p and 4K sky frames on lavapipe (minutes); run with --ignored, LOOK_CHECK_OUT=<dir>"]
#[cfg(unix)]
fn sky_at_4k() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip sky look check: {err}");
        return;
    }
    let dither_calls: Vec<(String, serde_json::Value)> = std::env::var("LOOK_CHECK_CALLS")
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<(String, serde_json::Value)>>(&text).ok())
        .unwrap_or_default();
    let tag = std::env::var("LOOK_CHECK_TAG").unwrap_or_else(|_| "before".into());
    let clear = sky_variant("look-sky-clear", &sky_ron(8.0, "fair", 0.0));
    let night = sky_variant("look-sky-night", &sky_ron(23.5, "fair", 0.0));
    let cloudy = sky_variant("look-sky-cloudy", &sky_ron(10.0, "cloudy", 0.6));
    for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
        let label = format!("{tag}_{w}x{h}");
        let (fw, fh, sky) = shot(
            &clear,
            (w, h),
            SKY_CAMERA,
            dither_calls.clone(),
            &format!("clear_{label}"),
            16,
        );
        let b = column_banding(fw, fh, &sky, fw / 2, 0..fh * 7 / 10, 8);
        println!(
            "LOOK sky_clear {label}: longest_run_px={} flat_share(run>=8px)={:.3} distinct_values={}",
            b.longest_run, b.flat_share, b.distinct
        );
        let (fw, fh, stars) = shot(
            &night,
            (w, h),
            SKY_CAMERA,
            dither_calls.clone(),
            &format!("night_{label}"),
            16,
        );
        let (count, peak) = star_stats(fw, fh, &stars, 12.0);
        let b = column_banding(fw, fh, &stars, fw / 2, 0..fh * 7 / 10, 8);
        println!(
            "LOOK sky_night {label}: stars={count} mean_peak_luma={peak:.1} longest_run_px={} flat_share={:.3}",
            b.longest_run, b.flat_share
        );
        // Toward the 08:00 sun (azimuth about 110 degrees, elevation 20): the sun disc and its forward-scattering
        // halo, where a coarse sky-view table shows its texels first.
        let (fw, fh, sun) = shot(
            &clear,
            (w, h),
            SUN_CAMERA,
            dither_calls.clone(),
            &format!("sun_{label}"),
            16,
        );
        let hf = high_frequency(fw, fh, &sun, (0, 0, fw, fh * 7 / 10));
        let b = column_banding(fw, fh, &sun, fw / 4, 0..fh * 7 / 10, 8);
        println!(
            "LOOK sky_sun {label}: high_freq_levels={hf:.3} longest_run_px={} flat_share={:.3}",
            b.longest_run, b.flat_share
        );
        let (fw, fh, clouds) = shot(
            &cloudy,
            (w, h),
            SKY_CAMERA,
            dither_calls.clone(),
            &format!("cloudy_{label}"),
            16,
        );
        let hf = high_frequency(fw, fh, &clouds, (0, 0, fw, fh * 7 / 10));
        let (mean, sd) = band_stats(fw, fh, &clouds, 0.0, 0.7);
        println!("LOOK sky_cloudy {label}: high_freq_levels={hf:.3} sky_mean={mean:.1} sky_sd={sd:.1}");
    }
}

#[test]
#[ignore = "look check: night stars at 1080p and 4K without temporal AA (the plain look); run with --ignored"]
fn night_stars_without_temporal_aa() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip night star check: {err}");
        return;
    }
    let night = sky_variant("look-night-plain", &sky_ron(23.5, "fair", 0.0));
    let ev: f32 = std::env::var("LOOK_NIGHT_EV")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7.0);
    for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
        let spec = JobSpec {
            width: w,
            height: h,
            frames: 1,
            warmup_frames: 16,
            plain: true,
            scatter_lod: 2,
            scene: Some(night.clone()),
            camera: Some(SKY_CAMERA),
            exposure_ev: Some(ev),
            keep: keep_dir(&format!("night_plain_{w}x{h}")),
            profile: Some(RenderProfilePreset::DesktopHigh),
            ..JobSpec::default()
        };
        let (fw, fh, rgba) = run(4, &spec).first_frame;
        let (count, peak) = star_stats(fw, fh, &rgba, 12.0);
        println!("LOOK night_plain {w}x{h} ev={ev}: stars={count} mean_peak_luma={peak:.1}");
    }
}

/// Pixels of the upper 70 % whose 13x13 mean luma is below `below`: at night under auto exposure the moonlit air
/// is pale and the clouds are dark, so this is the cloud mask (a star or two barely moves a 169-pixel mean).
fn dark_mask(w: u32, h: u32, rgba: &[u8], below: f32) -> Vec<bool> {
    let l: Vec<f32> = rgba.as_chunks::<4>().0.iter().map(|p| luma(p)).collect();
    let (wu, hu) = (w as usize, h as usize);
    // Summed-area table, one row and column of zeros in front.
    let mut sat = vec![0.0f64; (wu + 1) * (hu + 1)];
    for y in 0..hu {
        let mut row = 0.0f64;
        for x in 0..wu {
            row += f64::from(l[y * wu + x]);
            sat[(y + 1) * (wu + 1) + x + 1] = sat[y * (wu + 1) + x + 1] + row;
        }
    }
    let r = 6usize;
    let mut mask = vec![false; wu * hu];
    for y in 0..hu * 7 / 10 {
        for x in 0..wu {
            let (x0, y0) = (x.saturating_sub(r), y.saturating_sub(r));
            let (x1, y1) = ((x + r + 1).min(wu), (y + r + 1).min(hu));
            let sum = sat[y1 * (wu + 1) + x1] - sat[y0 * (wu + 1) + x1] - sat[y1 * (wu + 1) + x0]
                + sat[y0 * (wu + 1) + x0];
            let mean = sum / ((x1 - x0) * (y1 - y0)) as f64;
            mask[y * wu + x] = mean < f64::from(below);
        }
    }
    mask
}

#[test]
#[ignore = "look check: stars inside and outside the night clouds at 1080p; run with --ignored, LOOK_CHECK_OUT=<dir>"]
#[cfg(unix)]
fn night_stars_in_and_out_of_clouds() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip night cloud check: {err}");
        return;
    }
    let size = (1920u32, 1080u32);
    for (name, weather, coverage) in [("fair", "fair", 0.0f32), ("cloudy", "cloudy", 0.6)] {
        let night = sky_variant(
            &format!("look-night-clouds-{name}"),
            &sky_ron(23.5, weather, coverage),
        );
        let (w, h, rgba) = shot(
            &night,
            size,
            SKY_CAMERA,
            Vec::new(),
            &format!("night_clouds_{name}"),
            16,
        );
        let mask = dark_mask(w, h, &rgba, 100.0);
        let cloud = |x: u32, y: u32| mask[(y * w + x) as usize];
        let clear = |x: u32, y: u32| y < h * 7 / 10 && !mask[(y * w + x) as usize];
        let share = mask.iter().filter(|&&m| m).count() as f32 / (w * h * 7 / 10) as f32;
        let (in_cloud, in_peak) = star_stats_where(w, h, &rgba, 12.0, cloud);
        let (in_clear, clear_peak) = star_stats_where(w, h, &rgba, 12.0, clear);
        println!(
            "LOOK night_clouds {name} {w}x{h}: cloud share {share:.3}; stars in cloud {in_cloud} (peak {in_peak:.1}); \
             in clear sky {in_clear} (peak {clear_peak:.1})"
        );
    }
}

/// From the same spot toward the 08:00 sun (+X east, +Z north).
const SUN_CAMERA: ([f32; 3], [f32; 3]) = ([0.0, 420.0, 0.0], [880.0, 767.0, -320.0]);

/// The forest view of the example scene: down the valley over the scattered pines.
const FOREST_CAMERA: ([f32; 3], [f32; 3]) = ([60.0, 46.0, -170.0], [0.0, 36.0, -100.0]);

fn forest(
    profile: RenderProfilePreset,
    lite: Option<&str>,
    size: (u32, u32),
    keep: &str,
) -> (u32, u32, Vec<u8>) {
    forest_with(profile, lite, size, keep, Vec::new()).0
}

/// [`forest`] with extra agent calls; also returns the frame's hash.
fn forest_with(
    profile: RenderProfilePreset,
    lite: Option<&str>,
    size: (u32, u32),
    keep: &str,
    extra: Vec<(String, serde_json::Value)>,
) -> ((u32, u32, Vec<u8>), String) {
    let mut calls = lite.map_or_else(Vec::new, |tier| {
        vec![("render.set_forward_lite".to_string(), json!({ "tier": tier }))]
    });
    calls.extend(extra);
    let spec = JobSpec {
        width: size.0,
        height: size.1,
        frames: 1,
        warmup_frames: 4,
        plain: false,
        scatter_lod: 1,
        camera: Some(FOREST_CAMERA),
        keep: keep_dir(keep),
        calls,
        profile: Some(profile),
        ..JobSpec::default()
    };
    let outcome = run(1, &spec);
    let hash = outcome.hashes.first().cloned().unwrap_or_default();
    (outcome.first_frame, hash)
}

fn describe(label: &str, (w, h, rgba): &(u32, u32, Vec<u8>)) {
    let (sky_mean, sky_sd) = band_stats(*w, *h, rgba, 0.0, 0.25);
    let (ground_mean, ground_sd) = band_stats(*w, *h, rgba, 0.6, 1.0);
    let hf = high_frequency(*w, *h, rgba, (0, h * 6 / 10, *w, *h));
    println!(
        "LOOK forest {label}: sky_mean={sky_mean:.1} sky_sd={sky_sd:.1} ground_mean={ground_mean:.1} ground_sd={ground_sd:.1} ground_high_freq={hf:.2}"
    );
}

#[test]
#[ignore = "look check: the forest at three tiers on lavapipe; run with --ignored, LOOK_CHECK_OUT=<dir>"]
fn lite_tier_forest() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip lite tier look check: {err}");
        return;
    }
    let size = (1280, 720);
    describe(
        "desktop_high",
        &forest(
            RenderProfilePreset::DesktopHigh,
            None,
            size,
            "forest_desktop_high",
        ),
    );
    describe(
        "desktop_low",
        &forest(RenderProfilePreset::DesktopLow, None, size, "forest_desktop_low"),
    );
    describe(
        "deck",
        &forest(RenderProfilePreset::Deck, None, size, "forest_deck"),
    );
    describe(
        "mobile_low_full_shader",
        &forest(
            RenderProfilePreset::MobileLow,
            None,
            size,
            "forest_mobile_low_full_shader",
        ),
    );
    describe(
        "mobile_high_lite",
        &forest(
            RenderProfilePreset::MobileHigh,
            Some("high"),
            size,
            "forest_mobile_high_lite",
        ),
    );
    describe(
        "mobile_low_lite",
        &forest(
            RenderProfilePreset::MobileLow,
            Some("low"),
            size,
            "forest_mobile_low_lite",
        ),
    );
}

fn sun_range(range_m: Option<f32>) -> Vec<(String, serde_json::Value)> {
    vec![(
        "render.set_sun_shadow_range".to_string(),
        json!({ "range_m": range_m }),
    )]
}

/// Mean luma of the ground band (lower 40 %) and the share of its pixels darker than 0.8 of that mean: a
/// sunlit meadow with tree shadows has a dark tail the shadowless one lacks.
fn ground_shadow_share(w: u32, h: u32, rgba: &[u8]) -> (f32, f32) {
    let (mean, _) = band_stats(w, h, rgba, 0.6, 1.0);
    let rows = (h * 6 / 10)..h;
    let total = (rows.len() as u32 * w).max(1);
    let dark = rows
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| luma(&rgba[((y * w + x) * 4) as usize..]) < 0.8 * mean)
        .count();
    (mean, dark as f32 / total as f32)
}

#[test]
#[ignore = "look check: Mobile Low's forest with the sun shadow range capped at several distances; run with --ignored"]
fn mobile_low_sun_shadow_range() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip sun shadow range check: {err}");
        return;
    }
    for (label, range) in [
        ("default", None),
        ("40m", Some(40.0)),
        ("80m", Some(80.0)),
        ("150m", Some(150.0)),
        ("300m", Some(300.0)),
    ] {
        let ((w, h, rgba), _) = forest_with(
            RenderProfilePreset::MobileLow,
            None,
            (1280, 720),
            &format!("forest_mobile_low_shadow_{label}"),
            sun_range(range),
        );
        let (mean, dark) = ground_shadow_share(w, h, &rgba);
        println!("LOOK mobile_low_shadow_range {label}: ground_mean={mean:.1} dark_share={dark:.4}");
    }
}

/// ADR-3373 diagnostics for a backend where the test below fails at its first check (Metal: the
/// default frame and the frame with `render.set_sun_shadow_range` cleared hash differently): the
/// default frame twice, then the cleared one, with the pixels that differ. Two default frames that
/// differ are the frame's own nondeterminism on that backend; equal ones put the difference on the
/// call. `cargo test -p showcase --test look_checks -- --ignored --nocapture sun_shadow_range_diagnostics`.
#[test]
#[ignore = "diagnostics: prints the forest's hashes and differences, asserts nothing"]
fn sun_shadow_range_diagnostics() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip: {err}");
        return;
    }
    let size = (640, 360);
    let low = RenderProfilePreset::MobileLow;
    let ((_, _, first), first_hash) = forest_with(low, None, size, "forest_diag_default_1", Vec::new());
    let ((_, _, second), second_hash) = forest_with(low, None, size, "forest_diag_default_2", Vec::new());
    let ((_, _, off), off_hash) = forest_with(low, None, size, "forest_diag_off", sun_range(None));
    let stats = |a: &[u8], b: &[u8]| {
        let (mut count, mut worst) = (0usize, 0u8);
        for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
            if pa != pb {
                count += 1;
                worst = worst.max(pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
            }
        }
        format!("{count} pixels differ, largest {worst}")
    };
    println!(
        "default {first_hash}, default again {second_hash}: {}",
        stats(&first, &second)
    );
    println!(
        "default {first_hash}, range cleared {off_hash}: {}",
        stats(&first, &off)
    );
}

#[test]
fn sun_shadow_range_off_is_bit_identical_and_on_brings_back_mobile_low_tree_shadows() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip sun shadow range test: {err}");
        return;
    }
    let size = (640, 360);
    let low = RenderProfilePreset::MobileLow;
    let ((_, _, default), default_hash) =
        forest_with(low, None, size, "forest_shadow_range_default", Vec::new());
    let ((w, h, off), off_hash) = forest_with(low, None, size, "forest_shadow_range_off", sun_range(None));
    if cfg!(target_os = "macos") {
        // ADR-3373: on Metal the forest frame is not bit-stable run to run (the M5 drew the default
        // frame twice with 5 pixels a level apart; the cleared cap differed from it by 10 pixels,
        // also by one level), so "the same frame" is the same pixels within a level. Vulkan keeps
        // the hash. A cap that took effect would move the shadows over thousands of pixels.
        let (mut count, mut worst) = (0usize, 0u8);
        for (pa, pb) in default.as_chunks::<4>().0.iter().zip(off.as_chunks::<4>().0) {
            if pa != pb {
                count += 1;
                worst = worst.max(pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
            }
        }
        assert!(
            count <= 64 && worst <= 1,
            "no cap draws the default frame: {count} pixels differ, by up to {worst}"
        );
    } else {
        assert_eq!(default_hash, off_hash, "no cap draws the default frame");
    }
    let ((_, _, on), on_hash) = forest_with(low, None, size, "forest_shadow_range_80", sun_range(Some(80.0)));
    assert_ne!(default_hash, on_hash, "an 80 m cap changes Mobile Low's frame");
    let (off_mean, off_dark) = ground_shadow_share(w, h, &off);
    let (on_mean, on_dark) = ground_shadow_share(w, h, &on);
    println!(
        "LOOK sun_shadow_range 640x360: off mean {off_mean:.1} dark {off_dark:.4}; 80 m mean {on_mean:.1} dark {on_dark:.4}"
    );
    // The trees' shadows darken part of the meadow: more dark ground, a darker mean.
    assert!(
        on_dark > off_dark + 0.005,
        "dark ground share {off_dark} -> {on_dark}"
    );
    assert!(on_mean < off_mean, "ground mean {off_mean} -> {on_mean}");
}

#[test]
fn mobile_low_lite_forest_draws_a_scene() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip lite forest test: {err}");
        return;
    }
    let (w, h, rgba) = forest(
        RenderProfilePreset::MobileLow,
        Some("low"),
        (320, 180),
        "forest_mobile_low_lite_small",
    );
    let (sky_mean, _) = band_stats(w, h, &rgba, 0.0, 0.2);
    let (ground_mean, ground_sd) = band_stats(w, h, &rgba, 0.6, 1.0);
    // A day sky over a darker forest floor, and a floor with structure (terrain shading, trees), not one colour.
    assert!(sky_mean > 40.0, "the sky is lit: mean luma {sky_mean}");
    assert!(
        ground_mean > 3.0,
        "the ground is not black: mean luma {ground_mean}"
    );
    assert!(
        sky_mean > ground_mean,
        "sky {sky_mean} is brighter than the forest floor {ground_mean}"
    );
    assert!(ground_sd > 2.0, "the ground has structure: sd {ground_sd}");
}

/// One held sky frame at 640x360 with the given agent calls; returns (hash of the frame, banding of the centre
/// column, mean sky luma).
#[cfg(unix)]
fn small_sky(scene: &Path, calls: Vec<(String, serde_json::Value)>) -> (String, Banding, f32) {
    let spec = JobSpec {
        width: 640,
        height: 360,
        frames: 1,
        warmup_frames: 2,
        plain: false,
        scatter_lod: 2,
        scene: Some(scene.to_path_buf()),
        camera: Some(SKY_CAMERA),
        calls,
        profile: Some(RenderProfilePreset::DesktopHigh),
        ..JobSpec::default()
    };
    let outcome = run(2, &spec);
    let (w, h, rgba) = &outcome.first_frame;
    let banding = column_banding(*w, *h, rgba, w / 2, 0..h * 7 / 10, 8);
    let (mean, _) = band_stats(*w, *h, rgba, 0.0, 0.7);
    (outcome.hashes[0].clone(), banding, mean)
}

#[test]
#[cfg(unix)]
fn output_dither_off_is_bit_identical_and_on_breaks_the_sky_bands() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip output dither test: {err}");
        return;
    }
    let clear = sky_variant("look-dither", &sky_ron(8.0, "fair", 0.0));
    let set = |on: bool| vec![("render.set_output_dither".to_string(), json!({ "enabled": on }))];
    let (default_hash, default_bands, default_mean) = small_sky(&clear, Vec::new());
    let (off_hash, _, _) = small_sky(&clear, set(false));
    assert_eq!(default_hash, off_hash, "dither off draws the default frame");
    let (on_hash, on_bands, on_mean) = small_sky(&clear, set(true));
    assert_ne!(default_hash, on_hash, "dither on changes the frame");
    println!(
        "LOOK dither 640x360: off {default_bands:?} mean {default_mean:.2}; on {on_bands:?} mean {on_mean:.2}"
    );
    assert!(
        default_bands.longest_run > on_bands.longest_run,
        "the dither shortens the longest band: {default_bands:?} -> {on_bands:?}"
    );
    assert!(
        on_bands.flat_share < default_bands.flat_share,
        "{default_bands:?} -> {on_bands:?}"
    );
    // Zero-mean noise: the sky keeps its brightness.
    assert!(
        (on_mean - default_mean).abs() < 0.5,
        "{default_mean} -> {on_mean}"
    );
}

#[test]
fn star_flux_off_is_bit_identical_and_on_scales_the_stars_with_height() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip star flux test: {err}");
        return;
    }
    let night = sky_variant("look-star-flux", &sky_ron(23.5, "fair", 0.0));
    let night_shot = |calls: Vec<(String, serde_json::Value)>| {
        let spec = JobSpec {
            width: 640,
            height: 360,
            frames: 1,
            warmup_frames: 2,
            plain: false,
            scatter_lod: 2,
            scene: Some(night.clone()),
            camera: Some(SKY_CAMERA),
            calls,
            profile: Some(RenderProfilePreset::DesktopHigh),
            ..JobSpec::default()
        };
        let outcome = run(3, &spec);
        let (w, h, rgba) = &outcome.first_frame;
        (outcome.hashes[0].clone(), star_stats(*w, *h, rgba, 12.0))
    };
    let set = |on: bool| vec![("render.set_star_flux".to_string(), json!({ "enabled": on }))];
    let (default_hash, default_stars) = night_shot(Vec::new());
    let (off_hash, _) = night_shot(set(false));
    assert_eq!(default_hash, off_hash, "star flux off draws the default frame");
    // At 360 rows the gain is (360 / 1080)^2: every star a ninth as bright, so fewer clear the threshold.
    let (on_hash, on_stars) = night_shot(set(true));
    assert_ne!(default_hash, on_hash, "star flux changes a 360-row night frame");
    println!(
        "LOOK star_flux 640x360: off stars={} peak={:.1}; on stars={} peak={:.1}",
        default_stars.0, default_stars.1, on_stars.0, on_stars.1
    );
    // A ninth of the light never shows more stars.
    assert!(on_stars.0 <= default_stars.0, "{default_stars:?} -> {on_stars:?}");
}

#[cfg(test)]
mod metrics {
    use super::*;

    fn image(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                v.extend_from_slice(&[r, g, b, 255]);
            }
        }
        v
    }

    #[test]
    fn a_quantized_gradient_bands_and_a_dithered_one_does_not() {
        let (w, h) = (4u32, 400u32);
        // 40 levels over 400 rows: 10-pixel bands.
        let banded = image(w, h, |_, y| {
            let v = 100 + (y / 10) as u8;
            [v, v, v]
        });
        let b = column_banding(w, h, &banded, 1, 0..h, 8);
        assert_eq!(b.longest_run, 10);
        assert!((b.flat_share - 1.0).abs() < 1e-6);
        // The same ramp with a +-1 alternation: no run longer than 2.
        let dithered = image(w, h, |_, y| {
            let v = 100 + (y / 10) as u8 + (y % 2) as u8;
            [v, v, v]
        });
        let d = column_banding(w, h, &dithered, 1, 0..h, 8);
        assert!(d.longest_run <= 2, "{d:?}");
        assert!(d.flat_share < 1e-6);
    }

    #[test]
    fn stars_are_counted_once_each() {
        let (w, h) = (64u32, 64u32);
        let img = image(w, h, |x, y| {
            if (x, y) == (10, 10) || (x, y) == (40, 50) {
                [200, 200, 200]
            } else {
                [5, 5, 5]
            }
        });
        let (count, peak) = star_stats(w, h, &img, 12.0);
        assert_eq!(count, 2);
        assert!((peak - 200.0).abs() < 0.5);
    }

    #[test]
    fn high_frequency_tells_a_checker_from_a_flat_field() {
        let (w, h) = (32u32, 32u32);
        let flat = image(w, h, |_, _| [90, 90, 90]);
        let checker = image(w, h, |x, y| {
            if (x + y).is_multiple_of(2) {
                [60, 60, 60]
            } else {
                [120, 120, 120]
            }
        });
        assert!(high_frequency(w, h, &flat, (0, 0, w, h)) < 1e-6);
        assert!(high_frequency(w, h, &checker, (0, 0, w, h)) > 20.0);
    }
}
