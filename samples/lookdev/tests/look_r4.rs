// SPDX-License-Identifier: MIT-0
//! Look-director round 4 acceptance on lavapipe (`lookdev::look_r2` stand-ins), measured
//! against numbers taken from the UE v13 parity plates with the same masks:
//!
//! - Switchbacks mid: the floor is carved by a low sun, with lit and shadowed sand about
//!   three times apart in light and a shadowed area comparable to the plate's.
//! - Switchbacks haze: the map haze no longer flattens the far mesas (their local
//!   contrast with the preset haze stays close to clear air).
//! - Bore Twelve: the ice prints the plates' mid-grey, not a bright white plain.

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

fn render(device: &RenderDevice, planets: &mut R2Planets, shot: R2Shot, camera: CameraOverride) -> Vec<u8> {
    let (pixels, _) = render_shot(device, planets, shot, options(), camera)
        .unwrap_or_else(|err| panic!("{}: {err}", shot.name()));
    if std::env::var_os("REGIA_WRITE_EVIDENCE").is_some() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/look_r4_tests");
        let _ = std::fs::create_dir_all(&dir);
        let tag = if camera.haze_density.is_some() {
            "_clear"
        } else {
            ""
        };
        let _ = lookdev::default_look::write_png(
            &dir.join(format!("{}{tag}.png", shot.name())),
            options().width,
            options().height,
            &pixels,
        );
    }
    pixels
}

fn rect_lumas(pixels: &[u8], rect: [f32; 4]) -> Vec<f32> {
    let o = options();
    let (x0, x1) = (
        (rect[0] * o.width as f32) as u32,
        (rect[2] * o.width as f32) as u32,
    );
    let (y0, y1) = (
        (rect[1] * o.height as f32) as u32,
        (rect[3] * o.height as f32) as u32,
    );
    let mut out = Vec::new();
    for y in y0..y1.min(o.height) {
        for x in x0..x1.min(o.width) {
            let i = ((y * o.width + x) * 4) as usize;
            if let Some(px) = pixels.get(i..i + 4) {
                out.push(luma(px));
            }
        }
    }
    out
}

/// Otsu's threshold on display luma (0–255).
fn otsu(values: &[f32]) -> f32 {
    let mut hist = [0u32; 256];
    for v in values {
        hist[(*v as usize).min(255)] += 1;
    }
    let n = values.len() as f64;
    let total: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, h)| i as f64 * f64::from(*h))
        .sum();
    let (mut sum_b, mut w_b, mut best, mut t) = (0.0, 0.0, 0.0, 0.0);
    for (i, h) in hist.iter().enumerate() {
        w_b += f64::from(*h);
        if w_b == 0.0 {
            continue;
        }
        let w_f = n - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += i as f64 * f64::from(*h);
        let (m_b, m_f) = (sum_b / w_b, (total - sum_b) / w_f);
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > best {
            best = between;
            t = i as f32;
        }
    }
    t
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// (shadowed fraction, lit/shadowed light ratio) of a rect: Otsu splits the display luma,
/// and the ratio compares the class medians decoded with a 2.2 display gamma.
fn sun_and_shadow(pixels: &[u8], rect: [f32; 4]) -> (f32, f32) {
    let values = rect_lumas(pixels, rect);
    let t = otsu(&values);
    let (shadow, lit): (Vec<f32>, Vec<f32>) = values.iter().partition(|v| **v <= t);
    let fraction = shadow.len() as f32 / values.len().max(1) as f32;
    let linear = |v: f32| (v / 255.0).powf(2.2);
    let ratio = linear(median(lit)) / linear(median(shadow)).max(1.0e-4);
    (fraction, ratio)
}

/// Mean |luma − 7×7 box mean| over mean luma: edges and strata, not overall level.
fn local_contrast(pixels: &[u8], rect: [f32; 4]) -> f32 {
    let o = options();
    let (w, h) = (o.width as i32, o.height as i32);
    let at = |x: i32, y: i32| {
        let i = ((y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) * 4) as usize;
        luma(&pixels[i..i + 4])
    };
    let (x0, x1) = ((rect[0] * w as f32) as i32, (rect[2] * w as f32) as i32);
    let (y0, y1) = ((rect[1] * h as f32) as i32, (rect[3] * h as f32) as i32);
    let (mut dev, mut sum) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let mut box_sum = 0.0;
            for dy in -3..=3 {
                for dx in -3..=3 {
                    box_sum += at(x + dx, y + dy);
                }
            }
            let v = at(x, y);
            dev += (v - box_sum / 49.0).abs();
            sum += v;
        }
    }
    dev / sum.max(1.0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn switchbacks_mid_floor_is_carved_by_sun_and_shadow() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    let pixels = render(
        &device,
        &mut planets,
        R2Shot::SwitchbacksMid,
        CameraOverride::default(),
    );
    let (fraction, ratio) = sun_and_shadow(&pixels, [0.0, 0.45, 1.0, 1.0]);
    eprintln!("switchbacks_mid floor: shadowed {fraction:.2}, lit/shadowed {ratio:.2}");
    // UE v13 plate, same mask and measure: shadowed 0.67, lit/shadowed 3.2.
    // Round 3 (sun behind the camera at 50°): shadowed 0.06, lit/shadowed 1.6.
    assert!(
        ratio >= 3.0,
        "the floor is flat-lit: lit/shadowed light {ratio:.2} (UE plate 3.2)"
    );
    assert!(
        (0.335..=1.0).contains(&fraction),
        "the shadowed floor area {fraction:.2} is not within 2× of the UE plate's 0.67"
    );
}

#[test]
fn switchbacks_haze_keeps_the_far_mesas_edges() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    // Far mesa bands of the two hazy plates' framings.
    for (shot, rect) in [
        (R2Shot::SwitchbacksWide, [0.45, 0.03, 1.0, 0.3]),
        (R2Shot::DuskVista, [0.55, 0.0, 1.0, 0.3]),
    ] {
        let hazy = render(&device, &mut planets, shot, CameraOverride::default());
        let clear = render(
            &device,
            &mut planets,
            shot,
            CameraOverride {
                haze_density: Some(0.0),
                ..CameraOverride::default()
            },
        );
        let (with, without) = (local_contrast(&hazy, rect), local_contrast(&clear, rect));
        eprintln!(
            "{}: far mesa local contrast {with:.4} with the map haze, {without:.4} in clear air",
            shot.name()
        );
        // Round 3's 6.5e-5 /m haze kept 0.35 of the clear-air contrast (0.0061 vs 0.0175 on
        // switchbacks_wide at 960×540). The UE plates' far walls measure 0.046 (wide) and
        // 0.036 (dusk); the rest of that gap is surface detail on the stand-in mesas.
        assert!(
            with >= 0.9 * without,
            "{}: the map haze flattens the far mesas ({with:.4} vs {without:.4} in clear air)",
            shot.name()
        );
    }
}

#[test]
fn bore_twelve_ice_prints_the_ue_mid_grey() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut planets = R2Planets::default();
    // Median ground luma (lower 45 % of the frame) on the UE v13 plates.
    for (shot, ue) in [(R2Shot::BoreTwelveMid, 110.0_f32), (R2Shot::Cockpit, 131.6)] {
        let pixels = render(&device, &mut planets, shot, CameraOverride::default());
        let o = options();
        let ground = region_percentile(&pixels, o.width, o.height, [0.0, 0.55, 1.0, 1.0], 50.0);
        let off = ground / ue - 1.0;
        eprintln!(
            "{}: ground median luma {ground:.1} (UE {ue:.1}, {:+.0} %)",
            shot.name(),
            off * 100.0
        );
        // Round 3 (0.84-reflectance ice, averaging meter): 139.9 and 175.5 (+27 %, +33 %).
        assert!(
            off.abs() <= 0.15,
            "{}: ground median luma {ground:.1} is not within 15 % of the UE plate's {ue:.1}",
            shot.name()
        );
    }
}
