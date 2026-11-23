// SPDX-License-Identifier: MPL-2.0
//! Independent verification of `regia_asset_pipeline::astc_encode` (the pure-Rust ASTC encoder),
//! written by the verification lane through the public API only. The vendored astcenc decoder
//! (`regia_asset_pipeline::astc::decompress_astc_rgba8`, a black-box oracle) is the reference:
//! the module's own decoder round-trips by construction, so every claim about the bytes here is
//! checked against astcenc as well.

use regia_asset_pipeline::astc::decompress_astc_rgba8;
use regia_asset_pipeline::astc_encode::{
    BlockKind, BlockSize, ColorEndpointMode, Quality, decode_block, decode_image, encode_block, encode_image,
    inspect_block, try_encode_block,
};

const QUALITIES: [Quality; 3] = [Quality::Fast, Quality::Balanced, Quality::Thorough];

/// xorshift32: a deterministic stream (tests never depend on entropy).
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

/// Lays `blocks` (each one block of `size`) side by side in one row and decodes the strip with
/// astcenc, returning each block's texels in row-major order.
fn astcenc_decode_blocks(blocks: &[[u8; 16]], size: BlockSize) -> Vec<Vec<[u8; 4]>> {
    let (bw, bh) = (size.width(), size.height());
    let width = bw * blocks.len() as u32;
    let flat: Vec<u8> = blocks.iter().flat_map(|b| b.iter().copied()).collect();
    let pixels = decompress_astc_rgba8(width, bh, &flat, (bw, bh), false).expect("astcenc decode");
    (0..blocks.len())
        .map(|i| {
            let mut texels = Vec::with_capacity(size.texel_count());
            for y in 0..bh {
                for x in 0..bw {
                    let p = ((y * width + i as u32 * bw + x) * 4) as usize;
                    texels.push([pixels[p], pixels[p + 1], pixels[p + 2], pixels[p + 3]]);
                }
            }
            texels
        })
        .collect()
}

/// Largest per-channel difference between two texel lists.
fn worst_diff(a: &[[u8; 4]], b: &[[u8; 4]]) -> u8 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)))
        .max()
        .unwrap_or(0)
}

fn block_sse(a: &[[u8; 4]], b: &[[u8; 4]]) -> u64 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| {
            p.iter().zip(q).map(|(x, y)| {
                let d = i64::from(*x) - i64::from(*y);
                (d * d) as u64
            })
        })
        .sum()
}

/// A varied set of source blocks for one size: random RGBA, random opaque, two-colour,
/// smooth ramps in random directions, greyscale, alpha-only ramps and near-constant blocks.
fn source_blocks(size: BlockSize, rng: &mut Rng, per_kind: usize) -> Vec<Vec<[u8; 4]>> {
    let (w, h) = (size.width() as i32, size.height() as i32);
    let n = size.texel_count();
    let mut out = Vec::new();
    for _ in 0..per_kind {
        out.push(
            (0..n)
                .map(|_| [rng.byte(), rng.byte(), rng.byte(), rng.byte()])
                .collect(),
        );
        out.push(
            (0..n)
                .map(|_| [rng.byte(), rng.byte(), rng.byte(), 255])
                .collect(),
        );
        let (c0, c1) = (
            [rng.byte(), rng.byte(), rng.byte(), rng.byte()],
            [rng.byte(), rng.byte(), rng.byte(), rng.byte()],
        );
        out.push(
            (0..n)
                .map(|_| if rng.next() & 1 == 0 { c0 } else { c1 })
                .collect(),
        );
        let (dx, dy) = (i32::from(rng.byte() % 9) - 4, i32::from(rng.byte() % 9) - 4);
        let base = [rng.byte(), rng.byte(), rng.byte()];
        let mut ramp = Vec::with_capacity(n);
        for y in 0..h {
            for x in 0..w {
                let t = dx * x + dy * y;
                let c = |b: u8, k: i32| (i32::from(b) + t * k).clamp(0, 255) as u8;
                ramp.push([c(base[0], 3), c(base[1], -2), c(base[2], 1), 255]);
            }
        }
        out.push(ramp);
        let lo = rng.byte() / 2;
        out.push(
            (0..n)
                .map(|_| {
                    let v = lo.saturating_add(rng.byte() / 2);
                    [v, v, v, 255]
                })
                .collect(),
        );
        let rgb = [rng.byte(), rng.byte(), rng.byte()];
        out.push(
            (0..n)
                .map(|i| [rgb[0], rgb[1], rgb[2], (i * 255 / (n - 1)) as u8])
                .collect(),
        );
        let flat = [rng.byte(), rng.byte(), rng.byte(), rng.byte()];
        let mut near = vec![flat; n];
        let at = (rng.next() as usize) % n;
        near[at][0] = near[at][0].wrapping_add(1);
        out.push(near);
    }
    out
}

/// The core conformance claim, at a scale the delivered tests do not reach: every block the
/// encoder emits, for every size and quality over 1 512 varied source blocks, is a legal block
/// that astcenc decodes to the same texels as the module's decoder (within one level), and
/// never to the error colour.
#[test]
fn every_emitted_block_decodes_the_same_in_astcenc() {
    let mut rng = Rng(0x00A5_7C01);
    let mut total = 0usize;
    let mut exact = 0usize;
    for size in BlockSize::ALL {
        let sources = source_blocks(size, &mut rng, 18);
        for quality in QUALITIES {
            let blocks: Vec<[u8; 16]> = sources.iter().map(|t| encode_block(t, size, quality)).collect();
            let reference = astcenc_decode_blocks(&blocks, size);
            for ((block, src), theirs) in blocks.iter().zip(&sources).zip(&reference) {
                let info = inspect_block(block, size);
                assert_ne!(info.kind, BlockKind::Invalid, "{size:?} {quality:?} {src:?}");
                let ours = decode_block(block, size);
                let worst = worst_diff(&ours, theirs);
                assert!(
                    worst <= 1,
                    "{size:?} {quality:?} {info:?}: module and astcenc differ by {worst}"
                );
                exact += usize::from(worst == 0);
                total += 1;
            }
        }
    }
    assert_eq!(total, 4 * 3 * 18 * 7);
    println!("{exact}/{total} blocks bit-exact between the module decoder and astcenc");
}

/// The module decoder on blocks the encoder did not write: random 128-bit patterns. It must
/// never panic and must always return the footprint's texel count; every random block it
/// accepts as `Normal` (its supported subset) must decode as astcenc decodes it.
#[test]
fn random_bytes_the_decoder_accepts_match_astcenc() {
    let mut rng = Rng(0x0BAD_B10C);
    let mut accepted = 0usize;
    for size in BlockSize::ALL {
        let mut normal = Vec::new();
        for _ in 0..40_000 {
            let mut block = [0u8; 16];
            block.iter_mut().for_each(|b| *b = rng.byte());
            // Bias half the blocks into the subset: one partition, a direct endpoint mode.
            if rng.next() & 1 == 0 {
                block[1] &= !0b0001_1000; // partition count - 1 = 0 (bits 11..13)
                let cem = [0u8, 4, 8, 12][(rng.next() % 4) as usize];
                block[1] = (block[1] & 0b0001_1111) | ((cem & 0b111) << 5);
                block[2] = (block[2] & !1) | (cem >> 3);
            }
            let texels = decode_block(&block, size);
            assert_eq!(texels.len(), size.texel_count());
            if inspect_block(&block, size).kind == BlockKind::Normal {
                normal.push(block);
            }
        }
        assert!(
            normal.len() >= 200,
            "{size:?}: only {} random blocks in the subset",
            normal.len()
        );
        let reference = astcenc_decode_blocks(&normal, size);
        for (block, theirs) in normal.iter().zip(&reference) {
            let ours = decode_block(block, size);
            let worst = worst_diff(&ours, theirs);
            assert!(
                worst <= 1,
                "{size:?} block {block:02x?} {:?}: differs by {worst}",
                inspect_block(block, size)
            );
        }
        accepted += normal.len();
    }
    println!("{accepted} random in-subset blocks matched astcenc");
}

/// Void-extent blocks of every 8-bit grey level and 256 random colours decode exactly in
/// astcenc too (the delivered test checks one colour at 4x4).
#[test]
fn every_solid_colour_is_exact_in_astcenc_at_every_size() {
    let mut rng = Rng(0x5011_D000);
    let mut colours: Vec<[u8; 4]> = (0..=255u8).map(|v| [v, v, v, 255]).collect();
    colours.extend((0..256).map(|_| [rng.byte(), rng.byte(), rng.byte(), rng.byte()]));
    for size in BlockSize::ALL {
        let blocks: Vec<[u8; 16]> = colours
            .iter()
            .map(|&c| encode_block(&vec![c; size.texel_count()], size, Quality::Fast))
            .collect();
        let reference = astcenc_decode_blocks(&blocks, size);
        for ((block, colour), theirs) in blocks.iter().zip(&colours).zip(&reference) {
            assert_eq!(inspect_block(block, size).kind, BlockKind::VoidExtent);
            assert!(theirs.iter().all(|p| p == colour), "{size:?} {colour:?}");
        }
    }
}

/// `encode_image` is exactly `encode_block` over the row-major block walk with the image's
/// last row and column replicated into partial blocks.
#[test]
fn image_encode_equals_per_block_encode_with_edge_replication() {
    let mut rng = Rng(0x1A6E);
    let (w, h) = (19u32, 13u32);
    let rgba: Vec<u8> = (0..w * h * 4).map(|_| rng.byte()).collect();
    for size in BlockSize::ALL {
        let (bw, bh) = (size.width(), size.height());
        let image = encode_image(w, h, &rgba, size, Quality::Fast).expect("encode");
        let mut manual = Vec::new();
        for by in 0..h.div_ceil(bh) {
            for bx in 0..w.div_ceil(bw) {
                let mut texels = Vec::new();
                for y in 0..bh {
                    for x in 0..bw {
                        let px = (bx * bw + x).min(w - 1);
                        let py = (by * bh + y).min(h - 1);
                        let i = ((py * w + px) * 4) as usize;
                        texels.push([rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]);
                    }
                }
                manual
                    .extend_from_slice(&try_encode_block(&texels, size, Quality::Fast).expect("exact size"));
            }
        }
        assert_eq!(image, manual, "{size:?}");
    }
}

/// The search breadth claim: Thorough tries a superset of Balanced's endpoint modes and weight
/// ranges, and Balanced of Fast's. Least-squares iteration counts and tweak passes differ
/// between qualities, so a higher quality can lose a block (measured: at 8x8 Thorough's total
/// error is 0.02 % above Balanced's on this set). Bounds: Fast >= Balanced in total error,
/// Thorough within 0.5 % of Balanced in total, and no single block worse by more than 10 %.
#[test]
fn more_search_effort_does_not_lose_quality() {
    let mut rng = Rng(0x0E0F_F0E7);
    for size in BlockSize::ALL {
        let sources = source_blocks(size, &mut rng, 10);
        let mut totals = [0u64; 3];
        let mut thorough_lost = 0usize;
        for src in &sources {
            let sse: Vec<u64> = QUALITIES
                .iter()
                .map(|&q| block_sse(src, &decode_block(&encode_block(src, size, q), size)))
                .collect();
            for (t, s) in totals.iter_mut().zip(&sse) {
                *t += s;
            }
            thorough_lost += usize::from(sse[2] > sse[1]);
            assert!(
                sse[2] * 10 <= sse[1] * 11 + 10 * 64,
                "{size:?}: thorough {} vs balanced {}",
                sse[2],
                sse[1]
            );
        }
        println!(
            "{size:?}: fast/balanced/thorough SSE {totals:?}; thorough worse than balanced on {thorough_lost}/{} blocks",
            sources.len()
        );
        assert!(
            totals[0] >= totals[1],
            "{size:?}: fast/balanced/thorough SSE {totals:?}"
        );
        assert!(
            totals[2] * 200 <= totals[1] * 201,
            "{size:?}: fast/balanced/thorough SSE {totals:?}"
        );
    }
}

/// Greyscale content resolves to the luminance modes and decodes grey in astcenc too
/// (the luminance endpoint modes are where an R/G/B mix-up would show).
#[test]
fn grey_content_uses_luminance_modes_and_stays_grey_in_astcenc() {
    let mut rng = Rng(0x0000_64E7);
    for size in BlockSize::ALL {
        let n = size.texel_count();
        let opaque: Vec<[u8; 4]> = (0..n)
            .map(|i| {
                let v = (i * 200 / n) as u8 + rng.byte() % 8;
                [v, v, v, 255]
            })
            .collect();
        let translucent: Vec<[u8; 4]> = (0..n)
            .map(|i| {
                let v = (i * 200 / n) as u8;
                [v, v, v, 255 - v]
            })
            .collect();
        let blocks = [
            encode_block(&opaque, size, Quality::Balanced),
            encode_block(&translucent, size, Quality::Balanced),
        ];
        assert_eq!(
            inspect_block(&blocks[0], size).cem,
            Some(ColorEndpointMode::Luminance),
            "{size:?}"
        );
        assert_eq!(
            inspect_block(&blocks[1], size).cem,
            Some(ColorEndpointMode::LuminanceAlpha),
            "{size:?}"
        );
        for theirs in astcenc_decode_blocks(&blocks, size) {
            assert!(theirs.iter().all(|p| p[0] == p[1] && p[1] == p[2]), "{size:?}");
        }
        let theirs = &astcenc_decode_blocks(&blocks[..1], size)[0];
        assert!(theirs.iter().all(|p| p[3] == 255));
    }
}

/// `decode_image` crops partial edge blocks exactly as astcenc does.
#[test]
fn partial_image_decode_matches_astcenc() {
    let mut rng = Rng(0x000E_D6E5);
    for (w, h) in [(1u32, 1u32), (7, 3), (13, 17), (33, 9)] {
        let rgba: Vec<u8> = (0..w * h * 4).map(|_| rng.byte()).collect();
        for size in BlockSize::ALL {
            let blocks = encode_image(w, h, &rgba, size, Quality::Fast).expect("encode");
            let ours = decode_image(w, h, &blocks, size).expect("decode");
            let theirs =
                decompress_astc_rgba8(w, h, &blocks, (size.width(), size.height()), false).expect("astcenc");
            let worst = ours
                .iter()
                .zip(&theirs)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(worst <= 1, "{w}x{h} {size:?}: {worst}");
        }
    }
}
