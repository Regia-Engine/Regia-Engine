// SPDX-License-Identifier: MPL-2.0
//! `aerial_from_tables` (ADR-3294) on the real outdoor composite: the horizon seam between the
//! sky's last row and the farthest ground row shrinks with the switch on for the clear, overcast
//! and heavy-rain presets, and heavy rain moves the sky and the ground together. The plates are
//! drawn by `render_horizon_plate` (the outdoor passes of `outdoor_sky.wgsl` and `clouds.wgsl`
//! over a flat ground disc 120 km wide, the eye 150 m up), read back linear.
//!
//! `REGIA_AERIAL_PLATES=<dir>` writes the owner's plates (off | on, side by side) there.

use std::sync::OnceLock;

use regia_render::horizon_seam::{HorizonSeam, luminance};
use regia_render::{
    HorizonPlate, HorizonPlateImage, HorizonWeather, RenderDevice, TableAir, TimeOfDay, render_horizon_plate,
};

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| RenderDevice::headless().expect("headless device"))
}

fn plate(time: TimeOfDay, weather: HorizonWeather, mode: TableAir) -> HorizonPlateImage {
    render_horizon_plate(device(), &HorizonPlate::new(time, weather, mode)).expect("horizon plate")
}

fn seam(image: &HorizonPlateImage) -> HorizonSeam {
    image.seam.expect("a sky-over-ground edge")
}

fn sunset() -> TimeOfDay {
    TimeOfDay {
        hour: 17.6,
        ..TimeOfDay::default()
    }
}

/// sRGB bytes at a fixed exposure (a plain `x / (1 + x)` curve: a comparison plate, not the
/// engine's tonemap).
fn encode(rgb: &[[f32; 3]], exposure: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() * 4);
    for c in rgb {
        for v in c {
            let x = (v * exposure).max(0.0);
            let m = x / (1.0 + x);
            let s = if m <= 0.003_130_8 {
                12.92 * m
            } else {
                1.055 * m.powf(1.0 / 2.4) - 0.055
            };
            out.push((s.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
        out.push(255);
    }
    out
}

/// Off | on, side by side, at the off plate's exposure.
fn write_plate(name: &str, off: &HorizonPlateImage, on: &HorizonPlateImage) {
    let Some(dir) = std::env::var_os("REGIA_AERIAL_PLATES") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("plate dir");
    let (w, h) = (off.width, off.height);
    let exposure = 1.5 / off.region_luminance.0.max(1e-3);
    let (a, b) = (encode(&off.rgb, exposure), encode(&on.rgb, exposure));
    let row = (w * 4) as usize;
    let mut both = Vec::with_capacity(a.len() * 2);
    for y in 0..h as usize {
        both.extend_from_slice(&a[y * row..(y + 1) * row]);
        both.extend_from_slice(&b[y * row..(y + 1) * row]);
    }
    let tmp = dir.join(format!("{name}.png.tmp"));
    let file = std::fs::File::create(&tmp).expect("plate file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w * 2, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&both))
        .expect("write plate");
    std::fs::rename(&tmp, dir.join(format!("{name}.png"))).expect("rename plate");
}

/// The acceptance: on every preset the switch makes the seam smaller (both the full and the
/// lite entries), and off draws no table entry and no veil.
#[test]
fn the_tables_shrink_the_horizon_seam_on_every_preset() {
    for weather in HorizonWeather::ALL {
        let off = plate(TimeOfDay::default(), weather, TableAir::Off);
        let on = plate(TimeOfDay::default(), weather, TableAir::Full);
        let lite = plate(TimeOfDay::default(), weather, TableAir::Lite);
        let (s_off, s_on, s_lite) = (seam(&off), seam(&on), seam(&lite));
        eprintln!(
            "{}: seam off {:.4} (lum {:.4}), on {:.4} (lum {:.4}), lite {:.4}; sky {:?} ground off {:?} on {:?}",
            weather.name(),
            s_off.step,
            s_off.luminance_step,
            s_on.step,
            s_on.luminance_step,
            s_lite.step,
            s_off.sky,
            s_off.ground,
            s_on.ground,
        );
        assert!(
            s_on.step < s_off.step,
            "{}: on {s_on:?} vs off {s_off:?}",
            weather.name()
        );
        assert!(
            s_lite.step < s_off.step,
            "{}: lite {s_lite:?} vs off {s_off:?}",
            weather.name()
        );
        assert_eq!((off.table_air_frames, off.weather_veils), (0, 0));
        let falls = weather.extinction_per_m() > 0.0;
        assert_eq!(on.table_air_frames, 1);
        assert_eq!(on.weather_veils, u64::from(falls));
        assert_eq!(lite.weather_veils, u64::from(falls));
        if weather == HorizonWeather::Clear {
            write_plate("noon_clear", &off, &on);
        }
        if weather == HorizonWeather::HeavyRain {
            write_plate("noon_heavy_rain", &off, &on);
        }
    }
}

/// Heavy rain against overcast (the same deck, nothing falling). Off, the rain does not touch the
/// sky at all, and in heavy rain the sky and the ground end at different brightness. On, the rain
/// changes the sky too, and in heavy rain the sky's and the ground's mean luminance end within
/// 1.25× of each other, closer than off (both fade toward the same rain in-scatter), and the sky
/// and the ground change by factors within 1.25× of each other (ADR-3294 (11): the rain's
/// in-scatter is the sphere average of the horizon sky, the lit ground and the sun, shaded by the
/// deck).
#[test]
fn heavy_rain_moves_the_sky_and_the_ground_together() {
    let regions = |mode: TableAir| {
        let dry = plate(TimeOfDay::default(), HorizonWeather::Overcast, mode);
        let wet = plate(TimeOfDay::default(), HorizonWeather::HeavyRain, mode);
        (dry.region_luminance, wet.region_luminance)
    };
    let ((sky_dry_off, ground_dry_off), (sky_wet_off, ground_wet_off)) = regions(TableAir::Off);
    let ((sky_dry_on, ground_dry_on), (sky_wet_on, ground_wet_on)) = regions(TableAir::Full);
    let apart = |a: f32, b: f32| (a.max(1e-6) / b.max(1e-6)).ln().abs().exp();
    eprintln!(
        "overcast -> heavy rain, mean luminance cd/m^2: off sky {sky_dry_off:.1} -> {sky_wet_off:.1}, ground \
         {ground_dry_off:.1} -> {ground_wet_off:.1} (wet sky/ground {:.3}); on sky {sky_dry_on:.1} -> {sky_wet_on:.1}, \
         ground {ground_dry_on:.1} -> {ground_wet_on:.1} (wet sky/ground {:.3})",
        apart(sky_wet_off, ground_wet_off),
        apart(sky_wet_on, ground_wet_on),
    );
    assert!(
        (sky_wet_off / sky_dry_off.max(1e-6) - 1.0).abs() < 1e-3,
        "off: rain must not touch the sky ({sky_dry_off} -> {sky_wet_off})"
    );
    assert!(
        (sky_wet_on / sky_dry_on.max(1e-6) - 1.0).abs() > 0.01,
        "on: the rain reaches the sky ({sky_dry_on} -> {sky_wet_on})"
    );
    assert!(
        apart(sky_wet_on, ground_wet_on) < 1.25,
        "on: heavy rain sky {sky_wet_on} vs ground {ground_wet_on}"
    );
    assert!(
        apart(sky_wet_on, ground_wet_on) < apart(sky_wet_off, ground_wet_off),
        "on must bring the sky and the ground closer than off"
    );
    let sky_change = sky_wet_on / sky_dry_on.max(1e-6);
    let ground_change = ground_wet_on / ground_dry_on.max(1e-6);
    eprintln!("on: heavy rain scales the sky x{sky_change:.3}, the ground x{ground_change:.3}");
    assert!(
        apart(sky_change, ground_change) < 1.25,
        "on: the sky (x{sky_change}) and the ground (x{ground_change}) change together"
    );
}

/// The brief's "heavy rain dims the sky and the ground" under the plate's stratus deck (ADR-3294
/// (13)): the raining deck blocks the direct sun on the drops — the sun's share of the rain's
/// in-scatter is 0 and the ground half is diffuse-lit — so heavy rain under a deck ends darker
/// than the dry deck, not brighter.
#[test]
fn heavy_rain_under_a_deck_dims_the_sky_and_the_ground() {
    let dry = plate(TimeOfDay::default(), HorizonWeather::Overcast, TableAir::Full).region_luminance;
    let wet = plate(TimeOfDay::default(), HorizonWeather::HeavyRain, TableAir::Full).region_luminance;
    let (sky, ground) = (wet.0 / dry.0.max(1e-6), wet.1 / dry.1.max(1e-6));
    assert!(
        sky < 0.95 && ground < 0.95,
        "heavy rain must dim the sky (x{sky}) and the ground (x{ground})"
    );
}

/// At sunset the seam shrinks too, and the plates for the owner.
#[test]
fn sunset_plates_and_seam() {
    let off = plate(sunset(), HorizonWeather::Clear, TableAir::Off);
    let on = plate(sunset(), HorizonWeather::Clear, TableAir::Full);
    let (s_off, s_on) = (seam(&off), seam(&on));
    eprintln!(
        "sunset clear: seam off {:.4}, on {:.4}; sky L {:.3}",
        s_off.step,
        s_on.step,
        luminance(s_off.sky)
    );
    assert!(s_on.step < s_off.step, "sunset: on {s_on:?} vs off {s_off:?}");
    write_plate("sunset_clear", &off, &on);
}

/// Inside the tables' range and with nothing falling, the switch changes no ground pixel nearer
/// than 30 km by more than the half-float step (the same tables, the same slices).
#[test]
fn near_ground_is_unchanged_in_clear_air() {
    let off = plate(TimeOfDay::default(), HorizonWeather::Clear, TableAir::Off);
    let on = plate(TimeOfDay::default(), HorizonWeather::Clear, TableAir::Full);
    let mut checked = 0;
    for ((a, b), d) in off.rgb.iter().zip(&on.rgb).zip(&off.distance_m) {
        if *d < 30_000.0 {
            for (x, y) in a.iter().zip(b) {
                assert!(
                    (x - y).abs() <= 1e-3 * x.abs().max(1.0),
                    "{a:?} vs {b:?} at {d} m"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked} near ground pixels");
}
