// SPDX-License-Identifier: MPL-2.0
//! Bounded analytic checks for longitude-wrapped environment sun extraction.

use regia_core::glam::Vec3;
use regia_pt::env_map::{EnvMap, rotate_y};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 64;
const HOT: f32 = 1_000.0;
const DARK: f32 = 0.1;
const RADIUS: f32 = 0.08;
const CUT_MULTIPLIER: f32 = 100.0;
const MAX_SUN_SOLID_ANGLE: f32 = 0.05;

fn disc_map(center: [f32; 3]) -> EnvMap {
    let center = Vec3::from_array(center).normalize();
    EnvMap::from_fn(WIDTH, HEIGHT, |direction| {
        if Vec3::from_array(direction).dot(center) >= RADIUS.cos() {
            [HOT, HOT * 0.9, HOT * 0.8]
        } else {
            [DARK; 3]
        }
    })
}

/// Shift complete columns so the texel data are algebraically identical under longitude wrap.
fn rotate_columns(map: &EnvMap, shift: u32) -> EnvMap {
    let shift = shift % map.width;
    let mut rotated = EnvMap::constant(map.width, map.height, [0.0; 3]);
    for row in 0..map.height {
        for col in 0..map.width {
            let destination = (col + shift) % map.width;
            rotated.rgb[(row * map.width + destination) as usize] = map.rgb[(row * map.width + col) as usize];
        }
    }
    rotated
}

/// Independent area oracle: exact row band area in f64, summed over map texels.
fn integrated_luminance(map: &EnvMap) -> f64 {
    let height = f64::from(map.height);
    let width = f64::from(map.width);
    let d_phi = std::f64::consts::TAU / width;
    let mut sum = 0.0;
    for row in 0..map.height {
        let theta0 = std::f64::consts::PI * f64::from(row) / height;
        let theta1 = std::f64::consts::PI * f64::from(row + 1) / height;
        let row_area = (theta0.cos() - theta1.cos()) * d_phi;
        for col in 0..map.width {
            let [r, g, b] = map.rgb[(row * map.width + col) as usize];
            let luminance = 0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b);
            sum += luminance * row_area;
        }
    }
    sum
}

fn extract(mut map: EnvMap) -> (EnvMap, regia_pt::env_map::ExtractedSun) {
    let sun = map
        .extract_sun(CUT_MULTIPLIER, MAX_SUN_SOLID_ANGLE)
        .expect("compact bright disc is accepted")
        .expect("bright disc is extracted");
    (map, sun)
}

fn assert_close(label: &str, a: f32, b: f32, tolerance: f32) {
    let scale = a.abs().max(b.abs()).max(1.0e-6);
    assert!((a - b).abs() <= tolerance * scale, "{label}: {a} vs {b}");
}

fn assert_rotated_pair(center: [f32; 3], shift: u32) {
    let source = disc_map(center);
    let rotated = rotate_columns(&source, shift);
    let (mut residual_a, sun_a) = extract(source);
    let (mut residual_b, sun_b) = extract(rotated);

    // A column permutation preserves the exact per-row area oracle independently of the
    // production texel-area helper. Extraction must preserve that integrated radiance too.
    for (map, sun) in [(&mut residual_a, sun_a), (&mut residual_b, sun_b)] {
        let before = integrated_luminance(&disc_map(center));
        let after = integrated_luminance(map) + f64::from(sun.illuminance);
        assert!(
            (after - before).abs() <= before * 0.01,
            "map + extracted sun energy {after} vs {before}"
        );
    }

    assert_eq!(sun_a.texels, sun_b.texels);
    assert_close(
        "component solid angle",
        sun_a.solid_angle,
        sun_b.solid_angle,
        1.0e-6,
    );
    assert_close("illuminance", sun_a.illuminance, sun_b.illuminance, 1.0e-5);
    assert_close(
        "angular radius",
        sun_a.angular_radius,
        sun_b.angular_radius,
        1.0e-6,
    );

    let azimuth = std::f32::consts::TAU * shift as f32 / WIDTH as f32;
    let expected = rotate_y(sun_a.toward, -azimuth);
    let got = Vec3::from_array(sun_b.toward);
    let expected = Vec3::from_array(expected);
    assert!(got.dot(expected) > 1.0 - 2.0e-5, "{got:?} vs {expected:?}");
}

#[test]
fn compact_disc_extraction_is_invariant_across_longitude_seam_and_near_pole() {
    // Column 0's disc straddles the seam; shifting it by 32 columns moves it away. This is
    // an exact permutation, so only floating-point centroid math is tolerated.
    let seam_center = EnvMap::from_fn(WIDTH, HEIGHT, |d| d).texel_dir(0, HEIGHT / 2);
    assert_rotated_pair(seam_center, 32);

    // A compact cap centered in the first row exercises the small-area, near-pole case where
    // longitude columns subtend very little distance.
    let polar_center = EnvMap::from_fn(WIDTH, HEIGHT, |d| d).texel_dir(0, 0);
    assert_rotated_pair(polar_center, 32);
}

#[test]
fn broad_bright_belt_remains_rejected_by_the_existing_policy() {
    let mut belt = EnvMap::from_fn(WIDTH, HEIGHT, |direction| {
        if direction[1] > 0.5 { [HOT; 3] } else { [DARK; 3] }
    });
    let error = belt
        .extract_sun(2.0, MAX_SUN_SOLID_ANGLE)
        .expect_err("broad bright belt is not a compact sun");
    assert!(error.contains("not a sun disc"), "{error}");
}
