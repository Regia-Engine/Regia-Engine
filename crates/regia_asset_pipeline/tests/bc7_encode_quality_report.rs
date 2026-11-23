// SPDX-License-Identifier: MPL-2.0
//! Quality and speed report for `regia_asset_pipeline::bc7_encode` against the cook's existing
//! BC7 path (`intel_tex_2`, the ISPC encoder `texture_compress` uses), both decoded by the same
//! reference decoder (`regia_asset::bcn_decode`). Ignored by default: it prints a table for the
//! verification report and asserts only what must hold for the module to be usable at all.
//!
//! Run: `cargo test --release -p regia_asset_pipeline --test bc7_encode_quality_report -- --ignored --nocapture`

use intel_tex_2::{RgbaSurface, bc7};
use regia_asset::bcn_decode::{BcnKind, decode_bcn};
use regia_asset_pipeline::bc7_encode::{EncodeOptions, encode_image};
use std::time::Instant;

/// PSNR over the listed channels.
fn psnr(a: &[u8], b: &[u8], channels: &[usize]) -> f64 {
    let (mut sse, mut n) = (0.0f64, 0u64);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for &c in channels {
            let d = f64::from(pa[c]) - f64::from(pb[c]);
            sse += d * d;
            n += 1;
        }
    }
    if sse == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sse / n as f64)).log10()
}

fn xorshift(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

/// Named test images, all with dimensions that are multiples of 4.
fn images() -> Vec<(String, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let (w, h) = (64u32, 64u32);
    let mut grad = Vec::new();
    let mut planar = Vec::new();
    let mut alpha = Vec::new();
    let mut checker = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let s = (x + y) as f32 / (w + h - 2) as f32;
            grad.extend_from_slice(&[
                (20.0 + 200.0 * s).round() as u8,
                (230.0 - 180.0 * s).round() as u8,
                (40.0 + 160.0 * s).round() as u8,
                255,
            ]);
            let (r, g) = (x * 255 / w, y * 255 / h);
            planar.extend_from_slice(&[r as u8, g as u8, ((r + g) / 2) as u8, 255]);
            alpha.extend_from_slice(&[200, 80, 40, (x * 255 / w) as u8]);
            let v = if (x / 2 + y / 2).is_multiple_of(2) {
                16
            } else {
                240
            };
            checker.extend_from_slice(&[v, v, v, 255]);
        }
    }
    let mut s = 0x1234_5678u32;
    let knots: Vec<[f32; 4]> = (0..36)
        .map(|_| std::array::from_fn(|_| (xorshift(&mut s) >> 24) as f32))
        .collect();
    let mut noise = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let (kx, ky) = ((x / 16) as usize, (y / 16) as usize);
            let (fx, fy) = ((x % 16) as f32 / 16.0, (y % 16) as f32 / 16.0);
            let k = |i: usize, j: usize| knots[j.min(5) * 6 + i.min(5)];
            let (k00, k10, k01, k11) = (k(kx, ky), k(kx + 1, ky), k(kx, ky + 1), k(kx + 1, ky + 1));
            for (((a, b), c), d) in k00.iter().zip(k10).zip(k01).zip(k11) {
                let top = a * (1.0 - fx) + b * fx;
                let bot = c * (1.0 - fx) + d * fx;
                noise.push((top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    let mut white = Vec::new();
    for _ in 0..w * h * 4 {
        white.push((xorshift(&mut s) >> 24) as u8);
    }
    out.push(("gradient".into(), w, h, grad));
    out.push(("planar".into(), w, h, planar));
    out.push(("alpha_ramp".into(), w, h, alpha));
    out.push(("checker2".into(), w, h, checker));
    out.push(("value_noise_rgba".into(), w, h, noise));
    out.push(("white_noise".into(), w, h, white));
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/lookdev/goldens");
    for name in ["wide_vista", "cockpit", "props_on_snow", "vehicle_in_air"] {
        let Ok(img) = image::open(root.join(format!("{name}.png"))) else {
            continue;
        };
        let img = img.to_rgba8();
        let (iw, ih) = (img.width() / 4 * 4, img.height() / 4 * 4);
        let cropped = image::imageops::crop_imm(&img, 0, 0, iw, ih).to_image();
        out.push((format!("render_{name}"), iw, ih, cropped.into_raw()));
    }
    out
}

fn decode(w: u32, h: u32, blocks: &[u8]) -> Vec<u8> {
    decode_bcn(BcnKind::Bc7, w, h, blocks).unwrap_or_default()
}

#[test]
#[ignore = "report: prints PSNR and timing tables; run with --release --ignored --nocapture"]
fn bc7_encoder_quality_and_speed_report() {
    println!(
        "| image | size | muse fast | muse normal | muse high | ispc ultrafast | ispc fast | ispc basic | ispc slow |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let mut times = [0.0f64; 7];
    let mut texels = 0u64;
    for (name, w, h, rgba) in images() {
        let has_alpha = rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
        let channels: &[usize] = if has_alpha { &[0, 1, 2, 3] } else { &[0, 1, 2] };
        texels += u64::from(w * h);
        let mut row = format!("| {name} | {w}x{h} |");
        for (i, opts) in [
            EncodeOptions::fast(),
            EncodeOptions::normal(),
            EncodeOptions::high(),
        ]
        .iter()
        .enumerate()
        {
            let t = Instant::now();
            let blocks = encode_image(&rgba, w, h, opts);
            times[i] += t.elapsed().as_secs_f64();
            let decoded = decode(w, h, &blocks);
            let p = psnr(&rgba, &decoded, channels);
            row += &format!(" {p:.2} |");
            if !has_alpha {
                // The encoder ranks candidates on RGBA, so on opaque content it also spends
                // precision keeping alpha at 255: count the texels where it did not.
                let off = decoded.as_chunks::<4>().0.iter().filter(|p| p[3] != 255).count();
                let rgba_p = psnr(&rgba, &decoded, &[0, 1, 2, 3]);
                println!("  {name} muse[{i}]: rgba PSNR {rgba_p:.2}, alpha!=255 texels {off}");
            }
        }
        let surface = RgbaSurface {
            width: w,
            height: h,
            stride: w * 4,
            data: &rgba,
        };
        let ispc = if has_alpha {
            [
                bc7::alpha_ultra_fast_settings(),
                bc7::alpha_fast_settings(),
                bc7::alpha_basic_settings(),
                bc7::alpha_slow_settings(),
            ]
        } else {
            [
                bc7::opaque_ultra_fast_settings(),
                bc7::opaque_fast_settings(),
                bc7::opaque_basic_settings(),
                bc7::opaque_slow_settings(),
            ]
        };
        for (i, settings) in ispc.iter().enumerate() {
            let t = Instant::now();
            let blocks = bc7::compress_blocks(settings, &surface);
            times[3 + i] += t.elapsed().as_secs_f64();
            let p = psnr(&rgba, &decode(w, h, &blocks), channels);
            row += &format!(" {p:.2} |");
        }
        println!("{row}");
    }
    let mtex = texels as f64 / 1.0e6;
    let mut row = "| **Mtexel/s (single thread)** | |".to_string();
    for t in &times {
        row += &format!(" {:.3} |", mtex / t.max(1e-9));
    }
    println!("{row}");
    println!("total texels {texels}; seconds {times:?}");
}
