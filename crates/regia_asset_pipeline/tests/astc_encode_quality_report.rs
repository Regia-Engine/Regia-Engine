// SPDX-License-Identifier: MPL-2.0
//! Quality and speed report for `regia_asset_pipeline::astc_encode` (the pure-Rust encoder)
//! against the cook's existing ASTC path (the vendored astcenc, `crate::astc`), both decoded by
//! astcenc's decoder. Ignored by default: it prints a table for the verification report and
//! asserts only what must hold for the module to be usable at all.
//!
//! Run: `cargo test --release -p regia_asset_pipeline --test astc_encode_quality_report -- --ignored --nocapture`

use ctt_astcenc::Preset;
use regia_asset_pipeline::astc::{AstcContent, compress_astc_rgba8_preset, decompress_astc_rgba8};
use regia_asset_pipeline::astc_encode::{BlockSize, Quality, encode_image};
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

/// Named test images: synthetic 64x64 patterns plus the four lookdev reference renders.
fn images() -> Vec<(String, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let (w, h) = (64u32, 64u32);
    let (mut grad, mut planar, mut alpha, mut checker) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
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
    let white: Vec<u8> = (0..w * h * 4).map(|_| (xorshift(&mut s) >> 24) as u8).collect();
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
        out.push((
            format!("render_{name}"),
            img.width(),
            img.height(),
            img.into_raw(),
        ));
    }
    out
}

#[test]
#[ignore = "report: prints PSNR and timing tables; run with --release --ignored --nocapture"]
fn astc_encoder_quality_and_speed_report() {
    let ours = [Quality::Fast, Quality::Balanced, Quality::Thorough];
    let theirs = [Preset::Fastest, Preset::Fast, Preset::Medium, Preset::Thorough];
    let imgs = images();
    for size in [
        BlockSize::Block4x4,
        BlockSize::Block5x5,
        BlockSize::Block6x6,
        BlockSize::Block8x8,
    ] {
        let fp = (size.width(), size.height());
        println!("\n### {}x{} ({:.2} bpp)\n", fp.0, fp.1, size.bits_per_texel());
        println!(
            "| image | size | muse fast | muse balanced | muse thorough | astcenc fastest | astcenc fast | astcenc medium | astcenc thorough |"
        );
        println!("|---|---|---|---|---|---|---|---|---|");
        let mut times = [0.0f64; 7];
        let mut texels = 0u64;
        for (name, w, h, rgba) in &imgs {
            let has_alpha = rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
            let channels: &[usize] = if has_alpha { &[0, 1, 2, 3] } else { &[0, 1, 2] };
            let content = if has_alpha {
                AstcContent::Rgba
            } else {
                AstcContent::Rgb
            };
            texels += u64::from(*w) * u64::from(*h);
            let mut row = Vec::new();
            for (i, q) in ours.iter().enumerate() {
                let t = Instant::now();
                let blocks = encode_image(*w, *h, rgba, size, *q).expect("muse encode");
                times[i] += t.elapsed().as_secs_f64();
                let dec = decompress_astc_rgba8(*w, *h, &blocks, fp, false).expect("decode");
                let p = psnr(rgba, &dec, channels);
                assert!(p > 8.0, "{name} {q:?} {fp:?}: {p:.2} dB means a broken encode");
                row.push(p);
            }
            for (i, preset) in theirs.iter().enumerate() {
                let t = Instant::now();
                let blocks = compress_astc_rgba8_preset(*w, *h, rgba, fp, false, content, *preset)
                    .expect("astcenc encode");
                times[3 + i] += t.elapsed().as_secs_f64();
                let dec = decompress_astc_rgba8(*w, *h, &blocks, fp, false).expect("decode");
                row.push(psnr(rgba, &dec, channels));
            }
            let cells: Vec<String> = row.iter().map(|p| format!("{p:.2}")).collect();
            println!("| {name} | {w}x{h} | {} |", cells.join(" | "));
        }
        let rates: Vec<String> = times
            .iter()
            .map(|t| format!("{:.3}", texels as f64 / t.max(1e-9) / 1e6))
            .collect();
        println!("| **Mtexel/s** | | {} |", rates.join(" | "));
    }
}
