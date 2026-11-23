// SPDX-License-Identifier: MPL-2.0
//! Independent verification of `regia_asset_pipeline::bc7_encode` (written by the verification
//! lane, not by the module's author). Public API only; every block is decoded by
//! `regia_asset::bcn_decode`, which `tests/bcn_decode_roundtrip.rs` already checks against the
//! cook's separate ISPC encoder. Deterministic inputs only (fixed-seed xorshift).

use regia_asset::bcn_decode::{BcnKind, decode_bcn, decode_block};
use regia_asset_pipeline::bc7_encode::{EncodeOptions, block_mode, encode_block, encode_image};

fn xorshift(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

fn byte(s: &mut u32) -> u8 {
    (xorshift(s) >> 24) as u8
}

fn decode(block: &[u8; 16]) -> [[u8; 4]; 16] {
    decode_block(BcnKind::Bc7, block)
}

fn sse(a: &[[u8; 4]; 16], b: &[[u8; 4]; 16]) -> u64 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| p.iter().zip(q).map(|(x, y)| u64::from(x.abs_diff(*y)).pow(2)))
        .sum()
}

fn max_err(a: &[[u8; 4]; 16], b: &[[u8; 4]; 16]) -> u8 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)))
        .max()
        .unwrap_or(0)
}

/// A mix of block kinds a texture really contains: lines with noise, two-colour splits along a
/// random straight cut, independent alpha, and a few fully random blocks.
fn random_blocks(n: usize, seed: u32) -> Vec<[[u8; 4]; 16]> {
    let mut s = seed;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let block: [[u8; 4]; 16] = match i % 4 {
            0 => {
                let (a, b): ([u8; 4], [u8; 4]) = (
                    std::array::from_fn(|_| byte(&mut s)),
                    std::array::from_fn(|_| byte(&mut s)),
                );
                std::array::from_fn(|_| {
                    let t = f32::from(byte(&mut s)) / 255.0;
                    std::array::from_fn(|c| {
                        let v = f32::from(a[c]) * (1.0 - t) + f32::from(b[c]) * t;
                        let jitter = f32::from(byte(&mut s) % 7) - 3.0;
                        (v + jitter).clamp(0.0, 255.0) as u8
                    })
                })
            }
            1 => {
                let (a, b): ([u8; 4], [u8; 4]) = (
                    [byte(&mut s), byte(&mut s), byte(&mut s), 255],
                    [byte(&mut s), byte(&mut s), byte(&mut s), 255],
                );
                let (nx, ny, off) = (
                    i32::from(byte(&mut s)) - 128,
                    i32::from(byte(&mut s)) - 128,
                    i32::from(byte(&mut s)) - 128,
                );
                std::array::from_fn(|t| {
                    let (x, y) = ((t % 4) as i32 * 2 - 3, (t / 4) as i32 * 2 - 3);
                    if x * nx + y * ny > off / 16 { a } else { b }
                })
            }
            2 => {
                let base = [byte(&mut s), byte(&mut s), byte(&mut s)];
                std::array::from_fn(|t| {
                    let l = (t as u8) * 15;
                    [
                        base[0] / 2 + l / 2,
                        base[1],
                        base[2] / 2 + (255 - l) / 2,
                        byte(&mut s),
                    ]
                })
            }
            _ => std::array::from_fn(|_| std::array::from_fn(|_| byte(&mut s))),
        };
        out.push(block);
    }
    out
}

#[test]
fn search_quality_is_monotonic_fast_normal_high_on_random_blocks() {
    // The module documents that each quality's candidate set and pass trajectory is a
    // superset of the lower one's, so the decoded error can never rise with quality.
    let (fast, normal, high) = (
        EncodeOptions::fast(),
        EncodeOptions::normal(),
        EncodeOptions::high(),
    );
    for (i, block) in random_blocks(240, 0x5eed_0001).iter().enumerate() {
        let f = sse(block, &decode(&encode_block(block, &fast)));
        let n = sse(block, &decode(&encode_block(block, &normal)));
        let h = sse(block, &decode(&encode_block(block, &high)));
        assert!(n <= f, "block {i}: normal {n} > fast {f}");
        assert!(h <= n, "block {i}: high {h} > normal {n}");
    }
}

#[test]
fn every_emitted_block_uses_an_implemented_mode_and_decodes() {
    for opts in [
        EncodeOptions::fast(),
        EncodeOptions::normal(),
        EncodeOptions::high(),
    ] {
        for block in random_blocks(80, 0x5eed_0002) {
            let enc = encode_block(&block, &opts);
            let mode = block_mode(&enc);
            assert!(
                matches!(mode, Some(1 | 3 | 4 | 5 | 6 | 7)),
                "mode {mode:?} at {opts:?}"
            );
            if opts == EncodeOptions::fast() {
                assert_eq!(mode, Some(6));
            }
        }
    }
}

#[test]
fn every_grey_level_and_random_solid_colours_are_within_one_level() {
    // Exhaustive over the 256 opaque greys plus 64 random RGBA colours (the module's own
    // test samples four colours).
    let mut s = 0x5eed_0003u32;
    let mut colours: Vec<[u8; 4]> = (0..=255u8).map(|v| [v, v, v, 255]).collect();
    colours.extend((0..64).map(|_| [byte(&mut s), byte(&mut s), byte(&mut s), byte(&mut s)]));
    for opts in [EncodeOptions::fast(), EncodeOptions::normal()] {
        for c in &colours {
            let texels = [*c; 16];
            let got = decode(&encode_block(&texels, &opts));
            assert!(
                max_err(&texels, &got) <= 1,
                "colour {c:?} at {opts:?} decoded as {:?}",
                got[0]
            );
        }
    }
}

#[test]
fn two_flat_colours_split_by_a_straight_cut_are_near_exact_at_high() {
    // Mode 3 (7-bit + P-bit endpoints) holds two opaque flat colours per partition within one
    // level whenever the cut matches one of the 64 shapes; straight vertical and horizontal
    // halves (shapes 13 and 32-ish) always do.
    let mut s = 0x5eed_0004u32;
    for i in 0..32 {
        let a = [byte(&mut s), byte(&mut s), byte(&mut s), 255];
        let b = [byte(&mut s), byte(&mut s), byte(&mut s), 255];
        let texels: [[u8; 4]; 16] = std::array::from_fn(|t| {
            let side = if i % 2 == 0 { t % 4 < 2 } else { t / 4 < 2 };
            if side { a } else { b }
        });
        let got = decode(&encode_block(&texels, &EncodeOptions::high()));
        assert!(
            max_err(&texels, &got) <= 1,
            "split {i}: {a:?}/{b:?} max err {}",
            max_err(&texels, &got)
        );
    }
}

#[test]
fn independent_colour_channel_is_carried_by_a_rotated_dual_mode() {
    // Red ramps along x; green takes two values in a pattern (0x6b1d) that is neither a
    // two-subset partition shape nor the complement of one, so partitions cannot separate
    // it; blue and alpha are constant. Only a rotation that moves green (or red) into the
    // alpha slot gives it its own index set: modes 4/5 with rotation 1 or 2 must win, and
    // the block must come back near-exact. The module's own tests never force a rotation.
    const PATTERN: u16 = 0x6b1d;
    let texels: [[u8; 4]; 16] = std::array::from_fn(|t| {
        let g = if (PATTERN >> t) & 1 == 1 { 220 } else { 30 };
        [((t % 4) * 80) as u8, g, 128, 255]
    });
    let enc = encode_block(&texels, &EncodeOptions::high());
    let got = decode(&enc);
    let fast = decode(&encode_block(&texels, &EncodeOptions::fast()));
    let mode = block_mode(&enc);
    assert!(
        sse(&texels, &got) < sse(&texels, &fast),
        "full search no better than mode 6"
    );
    assert!(
        max_err(&texels, &got) <= 4,
        "max err {} with mode {mode:?}",
        max_err(&texels, &got)
    );
    let Some(m @ (4 | 5)) = mode else {
        panic!("expected a dual-index mode, got {mode:?}");
    };
    let rotation = (u128::from_le_bytes(enc) >> (u32::from(m) + 1)) & 3;
    assert!(matches!(rotation, 1 | 2), "mode {m} with rotation {rotation}");
}

#[test]
fn padded_texels_replicate_the_last_row_and_column() {
    // The module's partial-block test only checks the cropped decode; this checks the padding
    // itself: in a 5x5 image the last block column/row holds texel (4, y)/(x, 4) replicated.
    let (w, h) = (5u32, 5u32);
    let mut img = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let v = if x == 4 || y == 4 {
                [250, 20, 20, 255]
            } else {
                [20, 20, 250, 255]
            };
            img[i..i + 4].copy_from_slice(&v);
        }
    }
    let blocks = encode_image(&img, w, h, &EncodeOptions::high());
    assert_eq!(blocks.len(), 4 * 16);
    for (bi, chunk) in blocks.as_chunks::<16>().0.iter().enumerate() {
        let got = decode(chunk);
        let (bx, by) = (bi % 2, bi / 2);
        if bx == 1 || by == 1 {
            // Every texel of a padded block is the edge colour.
            for px in got {
                assert!(px[0] > 240 && px[2] < 30, "block {bi} padded texel {px:?}");
            }
        }
    }
    let full = decode_bcn(BcnKind::Bc7, w, h, &blocks).unwrap_or_default();
    assert_eq!(full.len(), img.len());
}

#[test]
fn image_encode_equals_per_block_encode() {
    // encode_image must be exactly encode_block over the row-major block walk.
    let (w, h) = (12u32, 8u32);
    let mut s = 0x5eed_0005u32;
    let img: Vec<u8> = (0..w * h * 4).map(|_| byte(&mut s)).collect();
    let opts = EncodeOptions::normal();
    let blocks = encode_image(&img, w, h, &opts);
    for by in 0..h / 4 {
        for bx in 0..w / 4 {
            let texels: [[u8; 4]; 16] = std::array::from_fn(|t| {
                let (x, y) = (bx * 4 + (t as u32 % 4), by * 4 + (t as u32 / 4));
                let i = ((y * w + x) * 4) as usize;
                [img[i], img[i + 1], img[i + 2], img[i + 3]]
            });
            let at = ((by * (w / 4) + bx) * 16) as usize;
            assert_eq!(
                &blocks[at..at + 16],
                &encode_block(&texels, &opts)[..],
                "block ({bx}, {by})"
            );
        }
    }
}

#[test]
fn zero_weight_channel_is_ignored_and_others_are_not_hurt() {
    // Weighting alpha to zero on an opaque-colour block with garbage alpha must give an RGB
    // error no worse than encoding the same RGB with opaque alpha at uniform weights... the
    // garbage alpha must not cost colour precision.
    let mut s = 0x5eed_0006u32;
    for i in 0..24 {
        let a = [byte(&mut s), byte(&mut s), byte(&mut s)];
        let b = [byte(&mut s), byte(&mut s), byte(&mut s)];
        let rgb: [[u8; 3]; 16] = std::array::from_fn(|t| {
            let f = t as f32 / 15.0;
            std::array::from_fn(|c| (f32::from(a[c]) * (1.0 - f) + f32::from(b[c]) * f).round() as u8)
        });
        let garbage: [[u8; 4]; 16] = std::array::from_fn(|t| [rgb[t][0], rgb[t][1], rgb[t][2], byte(&mut s)]);
        let opaque: [[u8; 4]; 16] = std::array::from_fn(|t| [rgb[t][0], rgb[t][1], rgb[t][2], 255]);
        let rgb_sse = |src: &[[u8; 4]; 16], got: &[[u8; 4]; 16]| -> u64 {
            src.iter()
                .zip(got)
                .map(|(p, q)| (0..3).map(|c| u64::from(p[c].abs_diff(q[c])).pow(2)).sum::<u64>())
                .sum()
        };
        let opts = EncodeOptions::high().with_channel_weights([1.0, 1.0, 1.0, 0.0]);
        let g = rgb_sse(&garbage, &decode(&encode_block(&garbage, &opts)));
        let o = rgb_sse(&opaque, &decode(&encode_block(&opaque, &EncodeOptions::high())));
        // A small slack: the two searches see different alpha, so tie-breaks may differ.
        assert!(
            g <= o + 16,
            "ramp {i}: rgb sse with ignored alpha {g} vs opaque {o}"
        );
    }
}

#[test]
fn eight_level_colour_with_two_level_alpha_uses_mode4_index_selection_1() {
    // Grey on the exact 3-bit interpolation levels between 0 and 255 (two texels a level) with
    // alpha in {0, 255} in a non-partition pattern. Mode 4 with the index-selection bit set
    // stores 3-bit colour and 2-bit alpha indices and is exact here; mode 5 has only 2-bit
    // colour indices (four levels), and no single line or partition fits. This forces the
    // encoder to choose, write and price index-selection 1 correctly.
    const LEVELS: [u8; 8] = [0, 36, 72, 108, 147, 183, 219, 255];
    const PATTERN: u16 = 0x6b1c;
    let texels: [[u8; 4]; 16] = std::array::from_fn(|t| {
        let v = LEVELS[t / 2];
        [v, v, v, if (PATTERN >> t) & 1 == 1 { 255 } else { 0 }]
    });
    let enc = encode_block(&texels, &EncodeOptions::high());
    let got = decode(&enc);
    assert_eq!(block_mode(&enc), Some(4), "mode");
    // Mode 4: five mode bits, two rotation bits, then the index-selection bit.
    let value = u128::from_le_bytes(enc);
    assert_eq!((value >> 7) & 1, 1, "index-selection bit");
    assert!(max_err(&texels, &got) <= 2, "max err {}", max_err(&texels, &got));
}
