// SPDX-License-Identifier: MPL-2.0
//! Android ASTC KTX2 container validation (`specs/12` § 7.1).

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{AstcBlock, TextureCookQuality, cook_texture_rgba8, write_ktx2_astc};

/// The default (high) quality cooks colour as ASTC 4×4: astcenc 4×4 matches BC7 at the same
/// 8 bits a texel, while 6×6 measured about 9 dB worse on DO27's terrain (`astc_survey`).
/// Low quality takes 6×6 where the size is whole 6×6 blocks.
#[test]
fn android_cook_texture_rgba8_produces_astc_ktx2() {
    let w = 12u32;
    let h = 12u32;
    let rgba = vec![255u8; (w * h * 4) as usize];
    for (quality, format) in [
        (TextureCookQuality::High, ktx2::Format::ASTC_4x4_SRGB_BLOCK),
        (TextureCookQuality::Low, ktx2::Format::ASTC_6x6_SRGB_BLOCK),
    ] {
        let cooked = cook_texture_rgba8(w, h, &rgba, TextureCookUsage::ColorSrgb, "android", quality)
            .expect("android astc cook");
        let reader = ktx2::Reader::new(&cooked.ktx2).expect("reader");
        assert_eq!(reader.header().format, Some(format), "{quality:?}");
        cooked.validate().expect("validates");
    }
}

#[test]
fn android_ktx2_astc_container_validates_with_ktx2_reader() {
    let w = 12u32;
    let h = 12u32;
    let blocks_x = w / 6;
    let blocks_y = h / 6;
    let block_bytes = (blocks_x * blocks_y * 16) as usize;
    let mips = vec![vec![0u8; block_bytes]];
    let ktx2 = write_ktx2_astc(w, h, AstcBlock::Block6x6, true, &mips).expect("write astc ktx2");
    let reader = ktx2::Reader::new(&ktx2).expect("reader");
    let fmt = reader.header().format.expect("format");
    assert_eq!(fmt, ktx2::Format::ASTC_6x6_SRGB_BLOCK);
    let tex = regia_asset::CookedTexture {
        width: w,
        height: h,
        usage: TextureCookUsage::ColorSrgb,
        ktx2: ktx2.into(),
    };
    tex.validate().expect("cooked texture validates");
}

/// A power-of-two texture (every real material map) cannot be 6×6 or 5×5 blocks: wgpu needs
/// level 0 in whole blocks. At the default quality every usage cooks ASTC 4×4; at low quality
/// colour and linear data take 8×8 (6×6 does not divide), masks 8×8 and normals stay 4×4.
/// Every chain runs down to 1×1: the tail levels smaller than a block are padded by the
/// encoder (they used to be dropped, so distant surfaces had no small mips).
#[test]
fn android_power_of_two_textures_cook_as_whole_block_astc() {
    let (w, h) = (256u32, 256u32);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i % 251) as u8, (i % 127) as u8, 90, 255])
        .collect();
    use TextureCookQuality::{High, Low};
    for (usage, quality, format) in [
        (
            TextureCookUsage::ColorSrgb,
            High,
            ktx2::Format::ASTC_4x4_SRGB_BLOCK,
        ),
        (
            TextureCookUsage::NormalMap,
            High,
            ktx2::Format::ASTC_4x4_UNORM_BLOCK,
        ),
        (
            TextureCookUsage::ColorLinear,
            High,
            ktx2::Format::ASTC_4x4_UNORM_BLOCK,
        ),
        (TextureCookUsage::Mask, High, ktx2::Format::ASTC_4x4_UNORM_BLOCK),
        (
            TextureCookUsage::ColorSrgb,
            Low,
            ktx2::Format::ASTC_8x8_SRGB_BLOCK,
        ),
        (
            TextureCookUsage::ColorLinear,
            Low,
            ktx2::Format::ASTC_8x8_UNORM_BLOCK,
        ),
        (
            TextureCookUsage::NormalMap,
            Low,
            ktx2::Format::ASTC_4x4_UNORM_BLOCK,
        ),
        (TextureCookUsage::Mask, Low, ktx2::Format::ASTC_8x8_UNORM_BLOCK),
    ] {
        let cooked = cook_texture_rgba8(w, h, &rgba, usage, "android", quality).expect("cook");
        let reader = ktx2::Reader::new(&cooked.ktx2).expect("reader");
        assert_eq!(reader.header().format, Some(format), "{usage:?} {quality:?}");
        assert_eq!(
            reader.levels().count() as u32,
            w.ilog2() + 1,
            "{usage:?} mip chain"
        );
        assert!(
            reader.levels().all(|l| l.data.len() >= 16),
            "{usage:?}: a level without a block"
        );
    }
}

/// The cooked blocks decode close to the source through astcenc's reference decoder: a colour
/// gradient keeps at least 40 dB, and a normal map's tail level (2×2, a partial 4×4 block)
/// still holds the source's direction.
#[test]
fn android_astc_cook_decodes_close_to_the_source() {
    use regia_asset_pipeline::astc::decompress_astc_rgba8;
    let (w, h) = (64u32, 64u32);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i % w * 4) as u8, (i / w * 4) as u8, 100, 255])
        .collect();
    let cooked = cook_texture_rgba8(
        w,
        h,
        &rgba,
        TextureCookUsage::ColorLinear,
        "android",
        TextureCookQuality::High,
    )
    .expect("cook");
    let reader = ktx2::Reader::new(&cooked.ktx2).expect("reader");
    let level0 = reader.levels().next().expect("level 0");
    let back = decompress_astc_rgba8(w, h, level0.data, (4, 4), false).expect("decode");
    let se: f64 = rgba
        .iter()
        .zip(&back)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum();
    let psnr = 10.0 * (255.0f64.powi(2) / (se / rgba.len() as f64).max(1e-9)).log10();
    assert!(psnr >= 40.0, "gradient PSNR {psnr:.1} dB");

    // A flat normal tilted towards +X (x = 0.6, y = 0): ASTC normals store X in RGB, Y in A.
    let normal: Vec<u8> = [204u8, 128, 230, 255].repeat((w * h) as usize);
    let cooked = cook_texture_rgba8(
        w,
        h,
        &normal,
        TextureCookUsage::NormalMap,
        "android",
        TextureCookQuality::High,
    )
    .expect("normal cook");
    let reader = ktx2::Reader::new(&cooked.ktx2).expect("reader");
    let tail = reader.levels().nth(5).expect("2x2 level");
    let back = decompress_astc_rgba8(2, 2, tail.data, (4, 4), false).expect("tail decode");
    for px in back.chunks(4) {
        assert!(
            px[0].abs_diff(204) <= 3 && px[3].abs_diff(128) <= 3,
            "tail texel {px:?}"
        );
    }
}

/// A desktop (BC) cook transcodes to the Android cook's ASTC, level for level: BC7 sRGB →
/// ASTC 4×4 sRGB, BC5 → ASTC 4×4 in the requested normal layout (terrain: `.rg`), BC4 →
/// ASTC 4×4; an ASTC blob passes through.
#[test]
fn desktop_cooks_transcode_to_android_astc() {
    use regia_asset_pipeline::astc::{AstcContent, decompress_astc_rgba8};
    use regia_asset_pipeline::transcode_bcn_ktx2_to_astc;
    let (w, h) = (32u32, 32u32);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i % w * 8) as u8, (i / w * 8) as u8, 60, 255])
        .collect();
    for (usage, format) in [
        (TextureCookUsage::ColorSrgb, ktx2::Format::ASTC_4x4_SRGB_BLOCK),
        (TextureCookUsage::NormalMap, ktx2::Format::ASTC_4x4_UNORM_BLOCK),
        (TextureCookUsage::Mask, ktx2::Format::ASTC_4x4_UNORM_BLOCK),
    ] {
        let bc = cook_texture_rgba8(w, h, &rgba, usage, "linux", TextureCookQuality::High).expect("bc cook");
        let astc = transcode_bcn_ktx2_to_astc(&bc.ktx2, TextureCookQuality::High, AstcContent::Rg)
            .expect("transcode");
        let reader = ktx2::Reader::new(&astc[..]).expect("reader");
        assert_eq!(reader.header().format, Some(format), "{usage:?}");
        let bc_levels = ktx2::Reader::new(&bc.ktx2[..]).expect("bc").levels().count();
        assert_eq!(reader.levels().count(), bc_levels, "{usage:?}: level for level");
        let level0 = reader.levels().next().expect("level 0");
        let back = decompress_astc_rgba8(w, h, level0.data, (4, 4), false).expect("decode");
        // Red follows x in every usage (normal: X in R; mask: R).
        let worst = rgba
            .chunks(4)
            .zip(back.chunks(4))
            .map(|(s, d)| s[0].abs_diff(d[0]))
            .max()
            .unwrap_or(0);
        assert!(worst <= 12, "{usage:?}: red error {worst}");
        if usage == TextureCookUsage::NormalMap {
            assert!(
                back.chunks(4).all(|p| p[2] == 0 && p[3] == 255),
                "terrain normals decode as .rg"
            );
        }
        // Already ASTC: unchanged.
        let again =
            transcode_bcn_ktx2_to_astc(&astc, TextureCookQuality::High, AstcContent::Rg).expect("pass");
        assert_eq!(again, astc);
    }
}
