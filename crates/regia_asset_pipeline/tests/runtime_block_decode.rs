// SPDX-License-Identifier: MPL-2.0
//! The runtime's CPU decoders for the Android formats (`regia_asset::astc_decode`,
//! `regia_asset::etc2_decode`, ADR-2063): ASTC against astcenc's reference decoder (the cook's
//! codec) bit for bit, and the round-trip PSNR floors of every Android format the cook writes
//! (the cook's ETC2 decoder is the runtime's). Deterministic inputs only (a fixed xorshift stream).

use regia_asset::astc_decode::{FOOTPRINTS, decode_astc};
use regia_asset::etc2_decode::{Etc2Format, decode_etc2};
use regia_asset_pipeline::astc::{AstcContent, compress_astc_rgba8, decompress_astc_rgba8};
use regia_asset_pipeline::etc2::{
    compress_eac_r11, compress_eac_rg11, compress_etc2_rgb8, compress_etc2_rgba8,
};

/// A fixed xorshift stream.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

/// A test image with every kind of block content: a smooth gradient (top left), hard-edged
/// colour patches (partitions), flat areas (void-extent blocks), noise (bottom right) and a
/// varying alpha.
fn test_image(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let n = rng.next();
            let px = match ((x * 4 / w), (y * 4 / h)) {
                (0 | 1, 0) => [
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    ((x + y) * 127 / (w + h)) as u8,
                    255,
                ],
                (2 | 3, 0) => {
                    if (x / 3 + y / 5) % 2 == 0 {
                        [220, 30, 40, 255]
                    } else {
                        [20, 90, 200, 255]
                    }
                }
                (_, 1) => [77, 160, 99, 255],
                (0 | 1, _) => [
                    (x * 9) as u8,
                    (n & 0x3f) as u8 + 100,
                    (y * 7) as u8,
                    ((x * 255 / w) as u8).max(16),
                ],
                _ => [n as u8, (n >> 8) as u8, (n >> 16) as u8, (n >> 24) as u8 | 0x80],
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

/// Every ASTC footprint, both endpoint expansions and every channel layout the cook encodes:
/// the runtime decoder returns exactly astcenc's texels (the encoder exercises the block modes,
/// partitions, dual planes and void extents).
#[test]
fn the_runtime_astc_decoder_matches_astcenc() {
    let contents = [
        AstcContent::Rgba,
        AstcContent::Rgb,
        AstcContent::Normal,
        AstcContent::Single,
        AstcContent::Rg,
    ];
    for (i, &(bw, bh)) in FOOTPRINTS.iter().enumerate() {
        // Not a whole number of blocks: the edge blocks are cropped.
        let (w, h) = (3 * bw + 3, 2 * bh + 5);
        for (j, &content) in contents.iter().enumerate() {
            for srgb in [false, true] {
                let rgba = test_image(w, h, 0x9e37_79b9 ^ (i as u32 * 31 + j as u32));
                let blocks = compress_astc_rgba8(w, h, &rgba, (bw, bh), srgb, content).expect("encode");
                let reference = decompress_astc_rgba8(w, h, &blocks, (bw, bh), srgb).expect("astcenc decode");
                let ours = decode_astc(w, h, (bw, bh), srgb, &blocks).expect("runtime decode");
                let diff = reference.iter().zip(&ours).position(|(a, b)| a != b).map(|at| {
                    (
                        at / 4,
                        &reference[at / 4 * 4..at / 4 * 4 + 4],
                        &ours[at / 4 * 4..at / 4 * 4 + 4],
                    )
                });
                assert_eq!(
                    diff, None,
                    "{bw}x{bh} {content:?} srgb {srgb}: first differing texel (index, astcenc, ours)"
                );
            }
        }
    }
}

/// Random 128-bit blocks (mostly reserved or invalid encodings, some valid ones of every
/// field combination) decode to astcenc's texels too, error colour included.
#[test]
fn random_astc_blocks_decode_like_astcenc() {
    let mut rng = Rng(0x2545_f491);
    let mut mismatches = Vec::new();
    for &(bw, bh) in &FOOTPRINTS {
        for srgb in [false, true] {
            let blocks: Vec<u8> = (0..400 * 16).map(|_| rng.next() as u8).collect();
            let (w, h) = (20 * bw, 20 * bh);
            let reference = decompress_astc_rgba8(w, h, &blocks, (bw, bh), srgb).expect("astcenc decode");
            let ours = decode_astc(w, h, (bw, bh), srgb, &blocks).expect("runtime decode");
            for (b, block) in blocks.as_chunks::<16>().0.iter().enumerate() {
                let (bx, by) = (b as u32 % 20, b as u32 / 20);
                let differs = (0..bh).any(|ty| {
                    let row = ((by * bh + ty) * w + bx * bw) as usize * 4;
                    let n = bw as usize * 4;
                    reference[row..row + n] != ours[row..row + n]
                });
                if differs && mismatches.len() < 8 {
                    mismatches.push(format!("{bw}x{bh} srgb {srgb} block {block:02x?}"));
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// PSNR (dB) over the `(source, decoded)` channel pairs.
fn psnr(source: &[u8], decoded: &[u8], channels: &[(usize, usize)]) -> f64 {
    let mut sum = 0f64;
    let mut n = 0f64;
    for (s, d) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for &(sc, dc) in channels {
            let e = f64::from(s[sc]) - f64::from(d[dc]);
            sum += e * e;
            n += 1.0;
        }
    }
    if sum == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
}

/// A smooth, photo-like test image (no noise): what the PSNR floors are set on.
fn smooth_image(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let r = 128.0 + 100.0 * (fx * 6.0).sin() * (fy * 3.0).cos();
            let g = 128.0 + 90.0 * ((fx + fy) * 4.0).cos();
            let b = 60.0 + 150.0 * fy;
            let a = 255.0 - 120.0 * fx;
            out.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
        }
    }
    out
}

/// Round trips through the cook's encoders and the runtime's decoders stay above a PSNR floor
/// per format: ASTC at every block the Android profiles use (4×4 to 8×8) and ETC2 / EAC. The
/// floors sit 2 to 3.5 dB under the values measured on 2026-11-01 (ASTC 4×4 44.5, 5×5 43.1, 6×6
/// 41.2, 8×8 37.6; EAC R11 51.4, RG11 49.9; ETC2 RGB8 49.7 with planar blocks, 37.2 without).
#[test]
fn android_round_trips_stay_above_their_psnr_floor() {
    let (w, h) = (64, 64);
    let rgba = smooth_image(w, h);
    let rgb = [(0, 0), (1, 1), (2, 2)];
    let rgba_ch = [(0, 0), (1, 1), (2, 2), (3, 3)];
    for ((bw, bh), floor) in [((4, 4), 42.0), ((5, 5), 40.5), ((6, 6), 38.5), ((8, 8), 35.0)] {
        for srgb in [false, true] {
            let blocks = compress_astc_rgba8(w, h, &rgba, (bw, bh), srgb, AstcContent::Rgba).expect("encode");
            let back = decode_astc(w, h, (bw, bh), srgb, &blocks).expect("decode");
            let db = psnr(&rgba, &back, &rgba_ch);
            assert!(
                db > floor,
                "ASTC {bw}x{bh} srgb {srgb}: {db:.1} dB (floor {floor})"
            );
        }
    }
    for (format, blocks, channels, floor) in [
        (Etc2Format::Rgb8, compress_etc2_rgb8(w, h, &rgba), &rgb[..], 46.0),
        (
            Etc2Format::Rgba8,
            compress_etc2_rgba8(w, h, &rgba),
            &rgba_ch[..],
            33.0,
        ),
        (
            Etc2Format::R11,
            compress_eac_r11(w, h, &rgba),
            &[(0, 0)][..],
            40.0,
        ),
        (
            Etc2Format::Rg11,
            compress_eac_rg11(w, h, &rgba),
            &[(0, 0), (1, 1)][..],
            40.0,
        ),
    ] {
        let blocks = blocks.expect("encode");
        let back = decode_etc2(format, w, h, &blocks).expect("decode");
        let db = psnr(&rgba, &back, channels);
        assert!(db > floor, "{format:?}: {db:.1} dB (floor {floor})");
    }
}
