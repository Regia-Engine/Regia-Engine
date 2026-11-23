// SPDX-License-Identifier: MPL-2.0
//! The runtime's CPU block decoder (`regia_asset::bcn_decode`, the fallback for GPUs without
//! BC) against the cook's encoder: every desktop texture format the cook writes decodes back to
//! its source within the format's error.

use regia_asset::TextureCookUsage;
use regia_asset::bcn_decode::{BcnKind, decode_bcn};
use regia_asset_pipeline::{TextureCookQuality, cook_texture_rgba8};

const S: u32 = 64;

/// Gradients, hard edges, noise and a soft alpha ramp: enough variety that the BC7 encoder picks
/// several modes (partitioned, rotated, alpha) across the blocks.
fn source(alpha: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            let h = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 7;
            let edge = if (x / 8 + y / 8) % 2 == 0 { 60 } else { 0 };
            out.extend_from_slice(&[
                ((x * 4) as u8).wrapping_add(edge),
                (y * 4) as u8,
                ((x + y) * 2) as u8 ^ (h as u8 & 15),
                if alpha { (x * 4) as u8 } else { 255 },
            ]);
        }
    }
    out
}

fn roundtrip(usage: TextureCookUsage, quality: TextureCookQuality, alpha: bool) -> (BcnKind, u8, f64) {
    let src = source(alpha);
    let cooked = cook_texture_rgba8(S, S, &src, usage, "linux", quality).expect("cook");
    let reader = ktx2::Reader::new(&cooked.ktx2).expect("ktx2");
    let kind = match reader.header().format.expect("format") {
        ktx2::Format::BC1_RGBA_SRGB_BLOCK
        | ktx2::Format::BC1_RGBA_UNORM_BLOCK
        | ktx2::Format::BC1_RGB_SRGB_BLOCK
        | ktx2::Format::BC1_RGB_UNORM_BLOCK => BcnKind::Bc1,
        ktx2::Format::BC3_SRGB_BLOCK | ktx2::Format::BC3_UNORM_BLOCK => BcnKind::Bc3,
        ktx2::Format::BC4_UNORM_BLOCK => BcnKind::Bc4,
        ktx2::Format::BC5_UNORM_BLOCK => BcnKind::Bc5,
        ktx2::Format::BC7_SRGB_BLOCK | ktx2::Format::BC7_UNORM_BLOCK => BcnKind::Bc7,
        other => panic!("unexpected cook format {other:?}"),
    };
    let level0 = reader.levels().next().expect("level 0").data;
    let decoded = decode_bcn(kind, S, S, level0).expect("decode");
    // Channels the format stores: BC4 R, BC5 RG, the rest RGB (+A when the source has it).
    let channels: &[usize] = match kind {
        BcnKind::Bc4 => &[0],
        BcnKind::Bc5 => &[0, 1],
        _ if alpha => &[0, 1, 2, 3],
        _ => &[0, 1, 2],
    };
    let (mut max, mut sum, mut n) = (0u8, 0u64, 0u64);
    for (a, b) in src.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for &c in channels {
            let d = a[c].abs_diff(b[c]);
            max = max.max(d);
            sum += u64::from(d);
            n += 1;
        }
    }
    (kind, max, sum as f64 / n as f64)
}

#[test]
fn cooked_block_formats_decode_back_to_their_source() {
    // (usage, quality, alpha, max bound, mean bound). Bounds: BC7 and BC4/BC5 interpolate 8-bit
    // endpoints finely; BC1/BC3 are 5:6:5 endpoints with 4 colours a block.
    let cases = [
        (
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
            false,
            40,
            3.0,
        ),
        (
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
            true,
            40,
            3.0,
        ),
        (
            TextureCookUsage::ColorLinear,
            TextureCookQuality::High,
            false,
            40,
            3.0,
        ),
        (
            TextureCookUsage::NormalMap,
            TextureCookQuality::High,
            false,
            24,
            2.0,
        ),
        (TextureCookUsage::Mask, TextureCookQuality::High, false, 24, 2.0),
        (
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::Low,
            false,
            64,
            8.0,
        ),
        (
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::Low,
            true,
            64,
            8.0,
        ),
    ];
    for (usage, quality, alpha, max_bound, mean_bound) in cases {
        let (kind, max, mean) = roundtrip(usage, quality, alpha);
        eprintln!("{usage:?} {quality:?} alpha={alpha}: {kind:?} max {max} mean {mean:.2}");
        assert!(
            max <= max_bound && mean <= mean_bound,
            "{usage:?} {quality:?} alpha={alpha} ({kind:?}): max {max} mean {mean:.2}"
        );
    }
}
