// SPDX-License-Identifier: MPL-2.0
//! Regression: a non-square image cooks a full mip chain down to 1x1.
//!
//! The mip generator and the KTX2 writer once stopped at the first edge that reached 1, so a
//! 16x4 image stored 3 levels (16x4, 8x2, 4x1) while the runtime and the transcoder size level
//! `i` as `(w >> i).max(1)` by `(h >> i).max(1)` and expect the chain to end at 1x1.

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{
    AstcBlock, BcnFormat, TextureCookQuality, cook_texture_rgba8, write_ktx2_astc, write_ktx2_bcn,
};

fn gradient(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| [(i * 7) as u8, (i * 3) as u8, (i % 251) as u8, 255])
        .collect()
}

fn level_count(width: u32, height: u32) -> u32 {
    width.max(height).ilog2() + 1
}

#[test]
fn desktop_cooks_of_non_square_images_have_a_full_chain() {
    for (w, h) in [(16u32, 4u32), (4, 16), (32, 8), (12, 4), (64, 4), (8, 4)] {
        let cooked = cook_texture_rgba8(
            w,
            h,
            &gradient(w, h),
            TextureCookUsage::ColorSrgb,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook");
        let reader = ktx2::Reader::new(&cooked.ktx2[..]).expect("reader");
        assert_eq!(reader.levels().count() as u32, level_count(w, h), "{w}x{h}");
        // Every level holds whole blocks of the right count: one block per 4x4 footprint.
        let block_bytes = reader.levels().last().expect("levels").data.len();
        for (i, level) in reader.levels().enumerate() {
            let (lw, lh) = ((w >> i).max(1), (h >> i).max(1));
            let blocks = (lw.div_ceil(4) * lh.div_ceil(4)) as usize;
            assert_eq!(level.data.len(), blocks * block_bytes, "{w}x{h} level {i}");
        }
    }
}

#[test]
fn android_cooks_of_non_square_images_have_a_full_chain() {
    for (w, h) in [(16u32, 4u32), (4, 32)] {
        let cooked = cook_texture_rgba8(
            w,
            h,
            &gradient(w, h),
            TextureCookUsage::ColorSrgb,
            "android",
            TextureCookQuality::High,
        )
        .expect("cook");
        let reader = ktx2::Reader::new(&cooked.ktx2[..]).expect("reader");
        assert_eq!(reader.levels().count() as u32, level_count(w, h), "{w}x{h}");
    }
}

#[test]
fn the_ktx2_writer_sizes_levels_below_one_texel_edge_as_one() {
    // 8x2 BC7: 8x2 (2 blocks), 4x1 (1), 2x1 (1), 1x1 (1).
    let mips = vec![vec![0u8; 32], vec![0u8; 16], vec![0u8; 16], vec![0u8; 16]];
    let blob = write_ktx2_bcn(8, 2, BcnFormat::Bc7Linear, &mips).expect("full chain");
    assert_eq!(ktx2::Reader::new(&blob).expect("reader").levels().count(), 4);
    // The same chain cut after the short edge reached 1 is still accepted.
    assert!(write_ktx2_bcn(8, 2, BcnFormat::Bc7Linear, &mips[..2]).is_ok());
    // A level of the wrong size past the short edge is refused.
    let mut bad = mips.clone();
    bad[3] = vec![0u8; 32];
    assert!(write_ktx2_bcn(8, 2, BcnFormat::Bc7Linear, &bad).is_err());
    // ASTC 6x6 on a 12x1 strip: 12x1 (2 blocks), 6x1 (1), 3x1 (1), 1x1 (1).
    let astc = vec![vec![0u8; 32], vec![0u8; 16], vec![0u8; 16], vec![0u8; 16]];
    assert!(write_ktx2_astc(12, 1, AstcBlock::Block6x6, false, &astc).is_ok());
}

#[test]
fn square_chains_are_unchanged() {
    let cooked = cook_texture_rgba8(
        16,
        16,
        &gradient(16, 16),
        TextureCookUsage::ColorSrgb,
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    assert_eq!(
        ktx2::Reader::new(&cooked.ktx2[..])
            .expect("reader")
            .levels()
            .count(),
        5
    );
}
