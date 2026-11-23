// SPDX-License-Identifier: MPL-2.0
//! BCn / ASTC / ETC2 texture compression and mip generation (`docs/specs/12-assets.md` § 7.1).

use intel_tex_2::{RSurface, RgSurface, RgbaSurface, bc1, bc3, bc5, bc7};
use regia_asset::{CookedTexture, TextureCookUsage};
use std::borrow::Cow;
use thiserror::Error;
use tracing::instrument;

pub(crate) use crate::astc_compress::AstcContent;
use crate::astc_compress::compress_astc_rgba8;
use crate::etc2_compress::{
    Etc2CompressError, Etc2Kind, compress_eac_r11, compress_eac_rg11, compress_etc2_rgb8, compress_etc2_rgba8,
};
use crate::ktx2_write::{
    AstcBlock, BcnFormat, Ktx2WriteError, write_ktx2_astc, write_ktx2_bcn, write_ktx2_etc2,
};

/// Desktop texture compression quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextureCookQuality {
    /// BC7 (or ASTC 4×4 on Android).
    #[default]
    High,
    /// BC1 for opaque sRGB and BC3 for sRGB with alpha. Linear data stays BC7, normals BC5
    /// and masks BC4. On Android, ASTC 6×6 / 8×8 (see `astc_block_for`); normals stay 4×4.
    Low,
}

/// Errors during texture cooking.
#[derive(Debug, Error)]
pub enum TextureCookError {
    /// KTX2 writer failure.
    #[error("ktx2 write error: {0}")]
    Ktx2(#[from] Ktx2WriteError),

    /// Dimensions not supported for block compression.
    #[error("texture {width}x{height} is too small to block-compress")]
    TooSmall {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },

    /// ASTC compression failure.
    #[error("astc compress error: {0}")]
    Astc(#[from] crate::astc_compress::AstcCompressError),

    /// The pixel buffer is not `width * height * 4` bytes.
    #[error("texture {width}x{height} needs {expected} RGBA8 bytes, got {actual}")]
    BadBuffer {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// Bytes an RGBA8 image of that size holds.
        expected: u64,
        /// Bytes given.
        actual: usize,
    },

    /// A cooked blob could not be transcoded ([`transcode_bcn_ktx2_to_astc`]).
    #[error("transcode: {0}")]
    Transcode(String),

    /// ETC2 / EAC compression failure (ADR-1379).
    #[error("etc2 compress error: {0}")]
    Etc2(#[from] Etc2CompressError),
}

/// The block format family an Android cook writes (ADR-1379).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidTextureFormat {
    /// ASTC (ADR-144), the default: 4×4 at the default quality, larger blocks at `Low`.
    #[default]
    Astc,
    /// ETC2 / EAC, for GPUs without ASTC: ETC2 RGB8 or RGBA8 for colour and linear data, EAC R11
    /// for one-channel masks, EAC RG11 for normal maps (X, Y; Z rebuilt like BC5). One bit rate:
    /// 4 bits a texel (8 with alpha or for normals); the quality setting does not change it.
    Etc2,
}

impl AndroidTextureFormat {
    /// `astc` or `etc2`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Astc => "astc",
            Self::Etc2 => "etc2",
        }
    }

    /// Parses [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "astc" => Some(Self::Astc),
            "etc2" => Some(Self::Etc2),
            _ => None,
        }
    }
}

/// [`cook_texture_rgba8`] for Android in the given block format family: `Astc` is exactly the
/// `platform = "android"` cook; `Etc2` writes ETC2 / EAC (ADR-1379).
///
/// # Errors
/// The image is smaller than one block, the buffer does not match the size, or encoding fails.
pub fn cook_texture_rgba8_android(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
    format: AndroidTextureFormat,
) -> Result<CookedTexture, TextureCookError> {
    if format == AndroidTextureFormat::Astc {
        // Explicitly ASTC at `quality`, whatever policy the caller's thread has.
        return crate::texture_policy::with_texture_policy(crate::TexturePolicy::default(), || {
            cook_texture_rgba8(width, height, rgba, usage, "android", quality)
        });
    }
    if width < 4 || height < 4 {
        return Err(TextureCookError::TooSmall { width, height });
    }
    check_rgba_len(width, height, rgba)?;
    let mips = generate_mips_rgba8(width, height, rgba);
    let ktx2 = cook_ktx2_etc2(width, height, &mips, usage)?;
    let ktx2 = stamp_detail(&ktx2, &mips)?;
    Ok(CookedTexture {
        width,
        height,
        usage,
        ktx2: ktx2.into(),
    })
}

/// The ETC2 / EAC kind and sRGB flag the Android ETC2 cook writes for `usage`.
pub(crate) fn etc2_kind_for(usage: TextureCookUsage, has_alpha: bool) -> (Etc2Kind, bool) {
    match usage {
        TextureCookUsage::NormalMap => (Etc2Kind::Rg11, false),
        TextureCookUsage::Mask => (Etc2Kind::R11, false),
        TextureCookUsage::ColorLinear if has_alpha => (Etc2Kind::Rgba8, false),
        TextureCookUsage::ColorLinear => (Etc2Kind::Rgb8, false),
        TextureCookUsage::ColorSrgb | TextureCookUsage::Ui if has_alpha => (Etc2Kind::Rgba8, true),
        TextureCookUsage::ColorSrgb | TextureCookUsage::Ui => (Etc2Kind::Rgb8, true),
    }
}

/// Compresses a mip chain (mip 0 first) to ETC2 / EAC KTX2 bytes.
pub(crate) fn cook_ktx2_etc2(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
) -> Result<Vec<u8>, TextureCookError> {
    let (kind, srgb) = etc2_kind_for(usage, has_alpha(mips));
    let _span = tracing::info_span!("etc2_compress_chain", levels = mips.len()).entered();
    let levels = mips
        .iter()
        .map(|(w, h, rgba)| match kind {
            Etc2Kind::Rgb8 => compress_etc2_rgb8(*w, *h, rgba),
            Etc2Kind::Rgba8 => compress_etc2_rgba8(*w, *h, rgba),
            Etc2Kind::R11 => compress_eac_r11(*w, *h, rgba),
            Etc2Kind::Rg11 => compress_eac_rg11(*w, *h, rgba),
        })
        .collect::<Result<Vec<_>, _>>()?;
    write_ktx2_etc2(width, height, kind, srgb, &levels).map_err(TextureCookError::from)
}

/// One texture of the Android format size report ([`android_format_report`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AndroidFormatRow {
    /// Texture width.
    pub width: u32,
    /// Texture height.
    pub height: u32,
    /// Bytes of the full RGBA8 mip chain (what an uncompressed upload costs).
    pub rgba8_bytes: u64,
    /// Bytes of the ASTC mip chain the default Android cook writes (payload, no KTX2 header).
    pub astc_bytes: u64,
    /// The ASTC block, `4x4`, `6x6` or `8x8`.
    pub astc_block: String,
    /// PSNR (dB) of the decoded ASTC level 0 against the source, over the channels the usage
    /// stores (RGB, RGBA with alpha, R for masks, RG for normals).
    pub astc_psnr_db: f64,
    /// Bytes of the ETC2 / EAC mip chain.
    pub etc2_bytes: u64,
    /// The VkFormat-style name of the ETC2 / EAC format.
    pub etc2_format: String,
    /// PSNR (dB) of the decoded ETC2 / EAC level 0, over the same channels.
    pub etc2_psnr_db: f64,
}

/// Measures one RGBA8 image for the Android size report (ADR-1379): the bytes of its full mip
/// chain as RGBA8, as the default ASTC cook writes it and as ETC2 / EAC, and the level-0 PSNR of
/// both encodings (decoded with astcenc's reference decoder and the ETC2 decoder in
/// `etc2_compress`). Deterministic for a given input.
///
/// # Errors
/// The image is smaller than one block, the buffer does not match the size, or encoding fails.
pub fn android_format_report(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> Result<AndroidFormatRow, TextureCookError> {
    use crate::astc_compress::decompress_astc_rgba8;
    use crate::etc2_compress::decode_etc2_rgba8;
    if width < 4 || height < 4 {
        return Err(TextureCookError::TooSmall { width, height });
    }
    check_rgba_len(width, height, rgba)?;
    let mips = generate_mips_rgba8(width, height, rgba);
    let alpha = has_alpha(&mips);
    let rgba8_bytes: u64 = mips
        .iter()
        .map(|(w, h, _)| u64::from(*w) * u64::from(*h) * 4)
        .sum();

    let block = astc_block_for(usage, quality, width, height);
    let content = match usage {
        TextureCookUsage::NormalMap => AstcContent::Normal,
        TextureCookUsage::Mask => AstcContent::Single,
        _ if alpha => AstcContent::Rgba,
        _ => AstcContent::Rgb,
    };
    let srgb = matches!(usage, TextureCookUsage::ColorSrgb | TextureCookUsage::Ui);
    let astc = compress_astc_mips(&mips, block, srgb, content)?;
    let astc_bytes = astc.iter().map(|l| l.len() as u64).sum();
    let astc_level0 = decompress_astc_rgba8(width, height, &astc[0], block.block_size(), srgb)?;

    let (kind, etc2_srgb) = etc2_kind_for(usage, alpha);
    let level0 = match kind {
        Etc2Kind::Rgb8 => compress_etc2_rgb8(width, height, rgba)?,
        Etc2Kind::Rgba8 => compress_etc2_rgba8(width, height, rgba)?,
        Etc2Kind::R11 => compress_eac_r11(width, height, rgba)?,
        Etc2Kind::Rg11 => compress_eac_rg11(width, height, rgba)?,
    };
    let etc2_bytes: u64 = mips
        .iter()
        .map(|(w, h, _)| u64::from(w.div_ceil(4)) * u64::from(h.div_ceil(4)) * kind.block_bytes() as u64)
        .sum();
    let etc2_level0 = decode_etc2_rgba8(kind, width, height, &level0)?;

    // ASTC normals decode X into R, G, B and Y into A; compare X and Y in both.
    let (astc_channels, etc2_channels): (ChannelPairs, ChannelPairs) = match usage {
        TextureCookUsage::NormalMap => (&[(0, 0), (1, 3)], &[(0, 0), (1, 1)]),
        TextureCookUsage::Mask => (&[(0, 0)], &[(0, 0)]),
        _ if alpha => (
            &[(0, 0), (1, 1), (2, 2), (3, 3)],
            &[(0, 0), (1, 1), (2, 2), (3, 3)],
        ),
        _ => (&[(0, 0), (1, 1), (2, 2)], &[(0, 0), (1, 1), (2, 2)]),
    };
    let etc2_format = match (kind, etc2_srgb) {
        (Etc2Kind::Rgb8, true) => "ETC2_R8G8B8_SRGB",
        (Etc2Kind::Rgb8, false) => "ETC2_R8G8B8_UNORM",
        (Etc2Kind::Rgba8, true) => "ETC2_R8G8B8A8_SRGB",
        (Etc2Kind::Rgba8, false) => "ETC2_R8G8B8A8_UNORM",
        (Etc2Kind::R11, _) => "EAC_R11_UNORM",
        (Etc2Kind::Rg11, _) => "EAC_R11G11_UNORM",
    };
    let (bw, bh) = block.block_size();
    Ok(AndroidFormatRow {
        width,
        height,
        rgba8_bytes,
        astc_bytes,
        astc_block: format!("{bw}x{bh}"),
        astc_psnr_db: psnr_mapped(rgba, &astc_level0, astc_channels),
        etc2_bytes,
        etc2_format: etc2_format.to_string(),
        etc2_psnr_db: psnr_mapped(rgba, &etc2_level0, etc2_channels),
    })
}

/// `(source channel, decoded channel)` pairs a PSNR compares.
type ChannelPairs = &'static [(usize, usize)];

/// PSNR (dB) between `source` and `decoded` RGBA8 images, comparing source channel `s` with
/// decoded channel `d` for each `(s, d)` pair; infinite when identical (reported as 99.0).
fn psnr_mapped(source: &[u8], decoded: &[u8], channels: &[(usize, usize)]) -> f64 {
    let mut sum = 0f64;
    let mut n = 0f64;
    for (s, d) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for &(sc, dc) in channels {
            let diff = f64::from(s[sc]) - f64::from(d[dc]);
            sum += diff * diff;
            n += 1.0;
        }
    }
    if sum == 0.0 || n == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
}

/// The size report as a fixed-width table with totals, for `etc2_survey` and logs.
#[must_use]
pub fn format_android_report(rows: &[(String, AndroidFormatRow)]) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut out = format!(
        "{:<32} {:>9} {:>10} {:>5} {:>8} {:>10} {:<20} {:>8}\n",
        "texture", "size", "ASTC MiB", "block", "ASTC dB", "ETC2 MiB", "ETC2 format", "ETC2 dB"
    );
    let (mut rgba8, mut astc, mut etc2) = (0u64, 0u64, 0u64);
    for (name, r) in rows {
        rgba8 += r.rgba8_bytes;
        astc += r.astc_bytes;
        etc2 += r.etc2_bytes;
        let size = format!("{}x{}", r.width, r.height);
        out.push_str(&format!(
            "{:<32} {:>9} {:>10.2} {:>5} {:>8.1} {:>10.2} {:<20} {:>8.1}\n",
            name,
            size,
            mib(r.astc_bytes),
            r.astc_block,
            r.astc_psnr_db,
            mib(r.etc2_bytes),
            r.etc2_format,
            r.etc2_psnr_db
        ));
    }
    out.push_str(&format!(
        "total: RGBA8 {:.2} MiB, ASTC {:.2} MiB, ETC2/EAC {:.2} MiB ({} textures)\n",
        mib(rgba8),
        mib(astc),
        mib(etc2),
        rows.len()
    ));
    out
}

/// Cook an RGBA8 image into a [`CookedTexture`] for the given platform tag.
#[instrument(skip(rgba), fields(w = width, h = height, platform = %platform))]
pub fn cook_texture_rgba8(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<CookedTexture, TextureCookError> {
    cook_texture_rgba8_with(width, height, rgba, usage, platform, quality, |_| {})
}

/// [`cook_texture_rgba8`] with alpha-coverage-preserving mips (ADR-3090, `crate::alpha_coverage`):
/// with `alpha_cutoff` set, every level after mip 0 is scaled so the fraction of texels at or above
/// the cutoff matches mip 0's, before the chain reaches the encoder (BC7 / BC3 on desktop, ASTC or
/// ETC2 / EAC on Android under the thread's texture policy). `None`, or a normal map, is exactly
/// [`cook_texture_rgba8`], byte for byte.
///
/// # Errors
/// As [`cook_texture_rgba8`].
pub fn cook_texture_rgba8_coverage(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
    alpha_cutoff: Option<f32>,
) -> Result<CookedTexture, TextureCookError> {
    let channel = crate::alpha_coverage::coverage_channel(usage);
    cook_texture_rgba8_with(width, height, rgba, usage, platform, quality, |chain| {
        if let (Some(cutoff), Some(channel)) = (alpha_cutoff, channel) {
            crate::alpha_coverage::preserve_coverage(chain, channel, cutoff);
        }
    })
}

/// [`cook_texture_rgba8`] with a hook that edits the generated mip chain (level 0 first,
/// each `(w, h, RGBA8)`) before compression: the packed ORM's normal-variance roughness.
pub(crate) fn cook_texture_rgba8_with(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
    adjust: impl FnOnce(&mut [(u32, u32, Vec<u8>)]),
) -> Result<CookedTexture, TextureCookError> {
    if width < 4 || height < 4 {
        return Err(TextureCookError::TooSmall { width, height });
    }
    check_rgba_len(width, height, rgba)?;

    let mut mips = generate_mips_rgba8(width, height, rgba);
    adjust(&mut mips);
    let ktx2 = cook_ktx2_with_policy(width, height, &mips, usage, platform, quality, None)?;
    let ktx2 = stamp_detail(&ktx2, &mips)?;
    Ok(CookedTexture {
        width,
        height,
        usage,
        ktx2: ktx2.into(),
    })
}

/// `ktx2` with the mip streaming metadata of its source chain `mips`.
fn stamp_detail(ktx2: &[u8], mips: &[(u32, u32, Vec<u8>)]) -> Result<Vec<u8>, TextureCookError> {
    let meta = regia_asset::MipStreamMeta::new(crate::mip_stream::detail_px(mips));
    crate::mip_stream::stamp_mip_stream(ktx2, &meta).map_err(TextureCookError::from)
}

/// `TextureCookError::BadBuffer` unless `rgba` is exactly `width * height * 4` bytes (the mip
/// generator indexes it as such and would panic on a short buffer).
fn check_rgba_len(width: u32, height: u32, rgba: &[u8]) -> Result<(), TextureCookError> {
    let expected = u64::from(width) * u64::from(height) * 4;
    if rgba.len() as u64 != expected {
        return Err(TextureCookError::BadBuffer {
            width,
            height,
            expected,
            actual: rgba.len(),
        });
    }
    Ok(())
}

/// [`cook_texture_rgba8`] with the Android ASTC channel layout forced to `content` (the desktop
/// BCn cook ignores it): a two-channel map the shader reads as `.rg` asks for [`AstcContent::Rg`].
pub(crate) fn cook_texture_rgba8_content(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
    content: AstcContent,
) -> Result<CookedTexture, TextureCookError> {
    if platform != "android" {
        return cook_texture_rgba8(width, height, rgba, usage, platform, quality);
    }
    if width < 4 || height < 4 {
        return Err(TextureCookError::TooSmall { width, height });
    }
    check_rgba_len(width, height, rgba)?;
    let mips = generate_mips_rgba8(width, height, rgba);
    let ktx2 = cook_ktx2_with_policy(width, height, &mips, usage, platform, quality, Some(content))?;
    let ktx2 = stamp_detail(&ktx2, &mips)?;
    Ok(CookedTexture {
        width,
        height,
        usage,
        ktx2: ktx2.into(),
    })
}

/// The generic texture cook's encoder dispatch: BCn on desktop; on Android the thread's
/// [`crate::TexturePolicy`] ([`crate::with_texture_policy`]) picks ETC2 / EAC or ASTC at the
/// profile's quality (`content` forces the ASTC channel layout; ETC2 lays channels out by usage,
/// and its RG11 normals are the `.rg` layout).
fn cook_ktx2_with_policy(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
    content: Option<AstcContent>,
) -> Result<Vec<u8>, TextureCookError> {
    if platform != "android" {
        return cook_ktx2_bcn(width, height, mips, usage, quality);
    }
    let policy = crate::texture_policy::active_texture_policy();
    match policy.android_format {
        AndroidTextureFormat::Etc2 => cook_ktx2_etc2(width, height, mips, usage),
        AndroidTextureFormat::Astc => {
            let quality = policy.astc_profile.quality(quality);
            cook_ktx2_astc_as(width, height, mips, usage, quality, content)
        }
    }
}

/// Compress an already-generated mip chain (mip 0 first) into KTX2 bytes, dispatching to the BCn
/// (desktop) or ASTC (Android) encoder. Used by [`cook_texture_rgba8`] and by the terrain tiled-texture
/// cooker, which pre-generates a shared mip chain and compresses the fine levels as independent tiles.
pub(crate) fn cook_ktx2_from_mips(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<Vec<u8>, TextureCookError> {
    if platform == "android" {
        cook_ktx2_astc(width, height, mips, usage, quality)
    } else {
        cook_ktx2_bcn(width, height, mips, usage, quality)
    }
}

fn cook_ktx2_bcn(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> Result<Vec<u8>, TextureCookError> {
    let (format, bcn_mips) = match usage {
        TextureCookUsage::NormalMap => {
            let rg = mips.iter().map(extract_rg_normal).collect::<Vec<_>>();
            (BcnFormat::Bc5, compress_bc5_chain(&rg))
        }
        TextureCookUsage::Mask => (BcnFormat::Bc4, compress_bc4_chain(mips)),
        TextureCookUsage::ColorLinear => (BcnFormat::Bc7Linear, compress_bc7_chain(mips, has_alpha(mips))),
        TextureCookUsage::ColorSrgb | TextureCookUsage::Ui => {
            let has_alpha = has_alpha(mips);
            let format = match quality {
                TextureCookQuality::High => BcnFormat::Bc7Srgb,
                TextureCookQuality::Low if has_alpha => BcnFormat::Bc3Srgb,
                TextureCookQuality::Low => BcnFormat::Bc1Srgb,
            };
            let bcn = match format {
                BcnFormat::Bc1Srgb => compress_bc1_chain(mips, true),
                BcnFormat::Bc3Srgb => compress_bc3_chain(mips, true),
                _ => compress_bc7_chain(mips, has_alpha),
            };
            (format, bcn)
        }
    };
    write_ktx2_bcn(width, height, format, &bcn_mips).map_err(TextureCookError::from)
}

fn cook_ktx2_astc(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> Result<Vec<u8>, TextureCookError> {
    cook_ktx2_astc_as(width, height, mips, usage, quality, None)
}

/// [`cook_ktx2_astc`] with the channel layout forced (`None`: the usage's own). The terrain
/// arrays cook their normals [`AstcContent::Rg`] (decoded like BC5, `.rg`) because the terrain
/// shader reads every normal array that way; material normal maps keep astcenc's normal mode
/// (`X X X Y`), which the forward shader decodes by layout code.
pub(crate) fn cook_ktx2_astc_as(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
    content: Option<AstcContent>,
) -> Result<Vec<u8>, TextureCookError> {
    let srgb = matches!(usage, TextureCookUsage::ColorSrgb | TextureCookUsage::Ui);
    let block = astc_block_for(usage, quality, width, height);
    let content = content.unwrap_or(match usage {
        TextureCookUsage::NormalMap => AstcContent::Normal,
        TextureCookUsage::Mask => AstcContent::Single,
        _ if has_alpha(mips) => AstcContent::Rgba,
        _ => AstcContent::Rgb,
    });
    let astc_mips = compress_astc_mips(mips, block, srgb, content)?;
    write_ktx2_astc(width, height, block, srgb, &astc_mips).map_err(TextureCookError::from)
}

/// The ASTC block the Android cook uses for a `width`×`height` texture of `usage` at
/// `quality`, chosen by measurement (`examples/astc_survey.rs`, DO27's terrain layer sets at
/// the mobile tiers' first sampled level; numbers in `docs/specs/12-assets.md` § 7.1):
///
/// - [`TextureCookQuality::High`] (the default): 4×4 for every usage. At the same 8 bits a
///   texel, astcenc's 4×4 matches or beats the desktop cook (colour 53.6 dB vs BC7 51.7, normals
///   1.7° mean error vs BC5 1.5°, ORM 50.2 dB vs BC7 47.6). 6×6 costs about 9 dB on colour (worst
///   texture 32.6 dB, visible on rock) and triples the normal error, for a saving of a few MiB
///   per layer array at the mobile tiers.
/// - [`TextureCookQuality::Low`]: 6×6 (3.56 bits) for colour and linear data, 8×8 (2 bits) for
///   one-channel masks, normals stay 4×4.
///
/// wgpu (WebGPU's rule) needs level 0 in whole blocks, so a power-of-two texture cannot take
/// 6×6 or 5×5: a block that does not divide the size steps down to 8×8, then 4×4. Lower levels
/// may end in partial blocks (the encoder pads them), so every chain runs down to 1×1.
pub(crate) fn astc_block_for(
    usage: TextureCookUsage,
    quality: TextureCookQuality,
    width: u32,
    height: u32,
) -> AstcBlock {
    let preferred = match (quality, usage) {
        (TextureCookQuality::High, _) | (TextureCookQuality::Low, TextureCookUsage::NormalMap) => {
            AstcBlock::Block4x4
        }
        (TextureCookQuality::Low, TextureCookUsage::Mask) => AstcBlock::Block8x8,
        (TextureCookQuality::Low, _) => AstcBlock::Block6x6,
    };
    let fits = |b: AstcBlock| {
        let (bw, bh) = b.block_size();
        width.is_multiple_of(bw) && height.is_multiple_of(bh)
    };
    // A Low texture whose size the preferred block does not divide keeps the low bit rate
    // (8×8) when it can, else 4×4 (which divides every power-of-two size from 4 up).
    let fallbacks: &[AstcBlock] = match preferred {
        AstcBlock::Block4x4 => &[],
        _ => &[AstcBlock::Block8x8, AstcBlock::Block4x4],
    };
    std::iter::once(preferred)
        .chain(fallbacks.iter().copied())
        .find(|&b| fits(b))
        .unwrap_or(preferred)
}

fn compress_astc_mips(
    mips: &[(u32, u32, Vec<u8>)],
    block: AstcBlock,
    srgb: bool,
    content: AstcContent,
) -> Result<Vec<Vec<u8>>, TextureCookError> {
    let _span = tracing::info_span!("astc_compress_chain", levels = mips.len()).entered();
    mips.iter()
        .map(|(w, h, rgba)| {
            compress_astc_rgba8(*w, *h, rgba, block.block_size(), srgb, content)
                .map_err(TextureCookError::from)
        })
        .collect()
}

/// Re-encodes a desktop-cooked KTX2 (BC1/BC3/BC4/BC5/BC7, any number of levels) as the Android
/// cook would have written it: every level decoded (`regia_asset::bcn_decode`) and encoded to
/// ASTC at the block the Android cook picks (`astc_block_for`, `specs/12` § 7.1) for the usage the BC format implies (BC7/BC1/BC3
/// sRGB → colour, their UNORM forms → linear data, BC5 → normal map, BC4 → one channel).
/// `normals` is the layout a BC5 normal map takes ([`AstcContent::Rg`] for terrain arrays, whose
/// shader reads `.rg`; [`AstcContent::Normal`] for material maps). The cook's own mips (Toksvig
/// roughness, tile-aware filtering) are kept, level by level.
///
/// For content cooked for desktop that has to run on a phone without a source re-cook (a shell's
/// pre-cooked terrain pack): the colour passes through BC once and ASTC once, so a re-cook from
/// the sources with `platform = "android"` is the better result when the sources are at hand.
/// An ASTC input is returned unchanged.
///
/// # Errors
///
/// The blob does not parse, uses a format without a decoder, or a level fails to decode/encode.
pub fn transcode_bcn_ktx2_to_astc(
    ktx2: &[u8],
    quality: TextureCookQuality,
    normals: AstcContent,
) -> Result<Vec<u8>, TextureCookError> {
    use ktx2::Format as F;
    use regia_asset::bcn_decode::{BcnKind, decode_bcn};
    let bad = |what: String| TextureCookError::Transcode(what);
    if let Some(reason) = regia_asset::hostile_ktx2_dfd(ktx2) {
        return Err(bad(reason.into()));
    }
    let reader = ktx2::Reader::new(ktx2).map_err(|e| bad(e.to_string()))?;
    let header = reader.header();
    let format = header
        .format
        .ok_or_else(|| bad("undefined (supercompressed) format".into()))?;
    let (kind, usage) = match format {
        F::BC1_RGB_SRGB_BLOCK | F::BC1_RGBA_SRGB_BLOCK => (BcnKind::Bc1, TextureCookUsage::ColorSrgb),
        F::BC1_RGB_UNORM_BLOCK | F::BC1_RGBA_UNORM_BLOCK => (BcnKind::Bc1, TextureCookUsage::ColorLinear),
        F::BC3_SRGB_BLOCK => (BcnKind::Bc3, TextureCookUsage::ColorSrgb),
        F::BC3_UNORM_BLOCK => (BcnKind::Bc3, TextureCookUsage::ColorLinear),
        F::BC7_SRGB_BLOCK => (BcnKind::Bc7, TextureCookUsage::ColorSrgb),
        F::BC7_UNORM_BLOCK => (BcnKind::Bc7, TextureCookUsage::ColorLinear),
        F::BC5_UNORM_BLOCK => (BcnKind::Bc5, TextureCookUsage::NormalMap),
        F::BC4_UNORM_BLOCK => (BcnKind::Bc4, TextureCookUsage::Mask),
        f if format!("{f:?}").starts_with("ASTC") => return Ok(ktx2.to_vec()),
        f => return Err(bad(format!("no decoder for {f:?}"))),
    };
    let (width, height) = (header.pixel_width.max(1), header.pixel_height.max(1));
    let mut mips = Vec::new();
    for (i, level) in reader.levels().enumerate() {
        let (w, h) = ((width >> i).max(1), (height >> i).max(1));
        let rgba = decode_bcn(kind, w, h, level.data).map_err(|e| bad(format!("level {i}: {e}")))?;
        mips.push((w, h, rgba));
    }
    let content = (usage == TextureCookUsage::NormalMap).then_some(normals);
    let astc = cook_ktx2_astc_as(width, height, &mips, usage, quality, content)?;
    // The source chain's metadata describes the same pixels.
    match regia_asset::MipStreamMeta::from_reader(&reader) {
        Some(meta) => crate::mip_stream::stamp_mip_stream(&astc, &meta).map_err(TextureCookError::from),
        None => Ok(astc),
    }
}

pub(crate) fn generate_mips_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let mut levels = vec![(width, height, rgba.to_vec())];
    let mut w = width;
    let mut h = height;
    while w >= 1 && h >= 1 && !(w == 1 && h == 1) {
        let next = downsample_box_rgba8(w, h, &levels[levels.len() - 1].2);
        // An edge that reached 1 stays 1 until the other does too: a 16x4 image has 5 levels.
        w = (w / 2).max(1);
        h = (h / 2).max(1);
        levels.push((w, h, next));
    }
    levels
}

fn downsample_box_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let nw = (width / 2).max(1);
    let nh = (height / 2).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let (y0, y1) = crate::texture_settings::box_window(y, nh, height);
        for x in 0..nw {
            let (x0, x1) = crate::texture_settings::box_window(x, nw, width);
            let n = (x1 - x0) * (y1 - y0);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * width + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += rgba[i + c] as u32;
                    }
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    out
}

fn extract_rg_normal(rgba: &(u32, u32, Vec<u8>)) -> (u32, u32, Vec<u8>) {
    let (w, h, data) = rgba;
    let mut rg = vec![0u8; (w * h * 2) as usize];
    for i in 0..(w * h) as usize {
        rg[i * 2] = data[i * 4];
        rg[i * 2 + 1] = data[i * 4 + 1];
    }
    (*w, *h, rg)
}

/// Whether the base level carries any alpha below 255.
fn has_alpha(mips: &[(u32, u32, Vec<u8>)]) -> bool {
    mips.first()
        .is_some_and(|(_, _, px)| px.chunks(4).any(|c| c[3] < 255))
}

/// Pads a mip level to whole 4×4 blocks by clamping to the edge texel. The block encoders
/// only visit complete blocks, so a 2×2 or 1×1 tail mip (or any size that is not a multiple of
/// 4) used to come out as all-zero blocks — black, transparent texels whenever a sampler
/// reached the smallest mips. Replication keeps the decoded texels of the real region exact.
pub(crate) fn pad_to_blocks(
    width: u32,
    height: u32,
    data: &[u8],
    bytes_per_texel: u32,
) -> (u32, u32, Cow<'_, [u8]>) {
    let pw = width.max(1).div_ceil(4) * 4;
    let ph = height.max(1).div_ceil(4) * 4;
    if pw == width && ph == height {
        return (width, height, Cow::Borrowed(data));
    }
    let bpt = bytes_per_texel as usize;
    let mut out = Vec::with_capacity((pw * ph) as usize * bpt);
    for y in 0..ph {
        let sy = y.min(height.max(1) - 1) as usize;
        for x in 0..pw {
            let sx = x.min(width.max(1) - 1) as usize;
            let i = (sy * width.max(1) as usize + sx) * bpt;
            out.extend_from_slice(&data[i..i + bpt]);
        }
    }
    (pw, ph, Cow::Owned(out))
}

/// Texels from which a level's blocks encode in parallel bands (the ASTC path's threshold in
/// `astc_compress`); smaller levels encode in one call.
const BANDED_MIN_TEXELS: u64 = 512 * 512;

std::thread_local! {
    /// Most band threads one level encode may use on this thread (`usize::MAX`: no cap). An
    /// outer encoder that already runs one job per thread sets it to 1 on its workers, so the
    /// inner block encoders do not spawn their own bands on top (the terrain tile cook).
    static BAND_THREAD_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// The calling thread's band-thread cap (see [`with_band_thread_cap`]).
pub(crate) fn band_thread_cap() -> usize {
    BAND_THREAD_CAP.with(std::cell::Cell::get)
}

/// Runs `f` with the calling thread's band-thread cap set to `cap` (at least 1), then restores
/// the previous cap. Applies to the BCn bands ([`band_threads`]) and the ASTC strips
/// (`astc_compress`), which are the same bytes at any band count.
pub(crate) fn with_band_thread_cap<R>(cap: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            BAND_THREAD_CAP.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(BAND_THREAD_CAP.with(|c| c.replace(cap.max(1))));
    f()
}

/// Threads to encode a padded `pw`×`ph` level with: one below [`BANDED_MIN_TEXELS`], else the
/// machine's parallelism (at most 16, at most one per block row, at most the calling thread's
/// [`band_thread_cap`]).
pub(crate) fn band_threads(pw: u32, ph: u32) -> usize {
    if u64::from(pw) * u64::from(ph) < BANDED_MIN_TEXELS {
        return 1;
    }
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16)
        .min((ph / 4) as usize)
        .min(band_thread_cap())
}

/// Encodes a padded level (`pw`×`ph` texels, whole 4×4 blocks, `bpt` bytes per texel) with
/// `encode(width, height, texels)`, cut into `threads` bands of whole block rows on scoped threads.
/// BCn blocks are independent and stored row-major, so the concatenated bands are the bytes of
/// one call over the whole level (ADR-530).
fn encode_banded(
    pw: u32,
    ph: u32,
    data: &[u8],
    bpt: u32,
    threads: usize,
    encode: impl Fn(u32, u32, &[u8]) -> Vec<u8> + Sync,
) -> Vec<u8> {
    let block_rows = ph / 4;
    if threads <= 1 || block_rows < 2 {
        return encode(pw, ph, data);
    }
    let band_rows = block_rows.div_ceil(threads as u32) * 4;
    let row_bytes = (pw * bpt) as usize;
    let encode = &encode;
    let bands: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let handles: Vec<_> = data
            .chunks(band_rows as usize * row_bytes)
            .map(|band| {
                let h = (band.len() / row_bytes) as u32;
                scope.spawn(move || encode(pw, h, band))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    bands.concat()
}

fn compress_bc7_chain(mips: &[(u32, u32, Vec<u8>)], with_alpha: bool) -> Vec<Vec<u8>> {
    // The opaque settings force alpha to 255; a texture that carries alpha (macro albedo with
    // AO in A) needs the alpha-aware mode search.
    let settings = if with_alpha {
        bc7::alpha_fast_settings()
    } else {
        bc7::opaque_fast_settings()
    };
    mips.iter()
        .map(|(w, h, rgba)| {
            let (pw, ph, data) = pad_to_blocks(*w, *h, rgba, 4);
            encode_banded(pw, ph, &data, 4, band_threads(pw, ph), |w, h, texels| {
                bc7::compress_blocks(&settings, &rgba_surface(w, h, texels))
            })
        })
        .collect()
}

/// An RGBA8 surface over `data` (`w`×`h`, tightly packed).
fn rgba_surface(w: u32, h: u32, data: &[u8]) -> RgbaSurface<'_> {
    RgbaSurface {
        width: w,
        height: h,
        data,
        stride: w * 4,
    }
}

fn compress_bc1_chain(mips: &[(u32, u32, Vec<u8>)], _srgb: bool) -> Vec<Vec<u8>> {
    mips.iter()
        .map(|(w, h, rgba)| {
            let (pw, ph, data) = pad_to_blocks(*w, *h, rgba, 4);
            encode_banded(pw, ph, &data, 4, band_threads(pw, ph), |w, h, texels| {
                bc1::compress_blocks(&rgba_surface(w, h, texels))
            })
        })
        .collect()
}

fn compress_bc3_chain(mips: &[(u32, u32, Vec<u8>)], _srgb: bool) -> Vec<Vec<u8>> {
    mips.iter()
        .map(|(w, h, rgba)| {
            let (pw, ph, data) = pad_to_blocks(*w, *h, rgba, 4);
            encode_banded(pw, ph, &data, 4, band_threads(pw, ph), |w, h, texels| {
                bc3::compress_blocks(&rgba_surface(w, h, texels))
            })
        })
        .collect()
}

fn compress_bc5_chain(mips: &[(u32, u32, Vec<u8>)]) -> Vec<Vec<u8>> {
    mips.iter()
        .map(|(w, h, rg)| {
            let (pw, ph, data) = pad_to_blocks(*w, *h, rg, 2);
            encode_banded(pw, ph, &data, 2, band_threads(pw, ph), |w, h, texels| {
                bc5::compress_blocks(&RgSurface {
                    width: w,
                    height: h,
                    data: texels,
                    stride: w * 2,
                })
            })
        })
        .collect()
}

fn compress_bc4_chain(mips: &[(u32, u32, Vec<u8>)]) -> Vec<Vec<u8>> {
    mips.iter()
        .map(|(w, h, rgba)| {
            let r: Vec<u8> = rgba.iter().step_by(4).copied().collect();
            let (pw, ph, data) = pad_to_blocks(*w, *h, &r, 1);
            encode_banded(pw, ph, &data, 1, band_threads(pw, ph), |w, h, texels| {
                intel_tex_2::bc4::compress_blocks(&RSurface {
                    width: w,
                    height: h,
                    data: texels,
                    stride: w,
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn mip_chain_shape_and_values_without_base_clone() {
        let base: Vec<u8> = (0..16 * 4 * 4).map(|i| (i % 251) as u8).collect();
        let mips = generate_mips_rgba8(16, 4, &base);
        let dims: Vec<(u32, u32)> = mips.iter().map(|m| (m.0, m.1)).collect();
        assert_eq!(dims, vec![(16, 4), (8, 2), (4, 1), (2, 1), (1, 1)]);
        assert_eq!(mips[0].2, base);
        for (w, h, d) in &mips {
            assert_eq!(d.len(), (w * h * 4) as usize);
        }
        assert_eq!(mips[1].2, downsample_box_rgba8(16, 4, &base));
        assert_eq!(mips[2].2, downsample_box_rgba8(8, 2, &mips[1].2));
    }

    use super::*;

    #[test]
    fn box_downsample_of_an_odd_edge_includes_the_last_column_and_row() {
        let px = |r: u8| [r, 0, 0, 255];
        // 5x1: samples average texels 0..2 and 2..5.
        let rgba: Vec<u8> = [px(10), px(10), px(0), px(0), px(60)].concat();
        let out = downsample_box_rgba8(5, 1, &rgba);
        assert_eq!((out[0], out[4]), (10, 20));
        // 1x3 (a column): the last sample also takes the remainder row.
        let rgba: Vec<u8> = [px(0), px(0), px(90)].concat();
        assert_eq!(downsample_box_rgba8(1, 3, &rgba)[0], 30);
    }

    #[test]
    fn a_short_or_ragged_buffer_is_an_error_not_a_panic() {
        for len in [0usize, 4, 16 * 16 * 4 - 1, 16 * 16 * 4 - 3, 16 * 16 * 4 + 4] {
            let rgba = vec![200u8; len];
            let err = cook_texture_rgba8(
                16,
                16,
                &rgba,
                TextureCookUsage::ColorSrgb,
                "linux",
                TextureCookQuality::High,
            )
            .expect_err("wrong buffer length");
            assert!(
                matches!(err, TextureCookError::BadBuffer { expected: 1024, actual, .. } if actual == len),
                "{len}: {err}"
            );
        }
    }

    /// `encode` over `threads` bands equals one call over the whole `w`×`h` level.
    fn bands_match(
        name: &str,
        w: u32,
        h: u32,
        texels: &[u8],
        bpt: u32,
        encode: impl Fn(u32, u32, &[u8]) -> Vec<u8> + Sync,
    ) {
        let whole = encode(w, h, texels);
        for threads in [2, 3, 5, 16] {
            assert_eq!(
                encode_banded(w, h, texels, bpt, threads, &encode),
                whole,
                "{name}, {threads} bands"
            );
        }
    }

    #[test]
    fn a_ragged_buffer_is_an_error_on_the_android_rg_path_too() {
        let rgba = vec![200u8; 16 * 16 * 4 - 1];
        let err = cook_texture_rgba8_content(
            16,
            16,
            &rgba,
            TextureCookUsage::NormalMap,
            "android",
            TextureCookQuality::High,
            AstcContent::Rg,
        )
        .expect_err("wrong buffer length");
        assert!(
            matches!(
                err,
                TextureCookError::BadBuffer {
                    expected: 1024,
                    actual: 1023,
                    ..
                }
            ),
            "{err}"
        );
    }

    /// Bands of whole block rows encode to exactly the bytes of one call over the level, for
    /// every BCn encoder the cook uses (so parallel encoding changes no cooked texture).
    #[test]
    fn banded_encoding_matches_one_call() {
        let (w, h) = (64u32, 64u32);
        let mut state = 0x2545_f491_u32;
        let rgba: Vec<u8> = (0..w * h * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        let rg: Vec<u8> = rgba
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 4 < 2)
            .map(|(_, &b)| b)
            .collect();
        let r: Vec<u8> = rgba.iter().step_by(4).copied().collect();
        let opaque = bc7::opaque_fast_settings();
        let alpha = bc7::alpha_fast_settings();
        bands_match("bc7 opaque", w, h, &rgba, 4, |w, h, t| {
            bc7::compress_blocks(&opaque, &rgba_surface(w, h, t))
        });
        bands_match("bc7 alpha", w, h, &rgba, 4, |w, h, t| {
            bc7::compress_blocks(&alpha, &rgba_surface(w, h, t))
        });
        bands_match("bc1", w, h, &rgba, 4, |w, h, t| {
            bc1::compress_blocks(&rgba_surface(w, h, t))
        });
        bands_match("bc3", w, h, &rgba, 4, |w, h, t| {
            bc3::compress_blocks(&rgba_surface(w, h, t))
        });
        bands_match("bc5", w, h, &rg, 2, |w, h, t| {
            bc5::compress_blocks(&RgSurface {
                width: w,
                height: h,
                data: t,
                stride: w * 2,
            })
        });
        bands_match("bc4", w, h, &r, 1, |w, h, t| {
            intel_tex_2::bc4::compress_blocks(&RSurface {
                width: w,
                height: h,
                data: t,
                stride: w,
            })
        });
        // Small levels stay one call.
        assert_eq!(band_threads(256, 256), 1);
    }

    /// A solid colour encodes to the same block at every level, including the 2×2 and 1×1
    /// tail mips. The encoders only visit whole 4×4 blocks, so before padding those levels
    /// came out all-zero (black, transparent) and distant terrain sampled black.
    #[test]
    fn sub_block_mips_encode_the_texels_not_zero() {
        let rgba: Vec<u8> = [90u8, 140, 60, 200].repeat(32 * 32);
        for (usage, quality) in [
            (TextureCookUsage::ColorSrgb, TextureCookQuality::High),
            (TextureCookUsage::ColorSrgb, TextureCookQuality::Low),
            (TextureCookUsage::NormalMap, TextureCookQuality::High),
            (TextureCookUsage::Mask, TextureCookQuality::High),
            (TextureCookUsage::ColorLinear, TextureCookQuality::High),
        ] {
            let cooked = match cook_texture_rgba8(32, 32, &rgba, usage, "linux", quality) {
                Ok(cooked) => cooked,
                Err(err) => panic!("{usage:?}: {err}"),
            };
            let reader = match ktx2::Reader::new(&cooked.ktx2[..]) {
                Ok(reader) => reader,
                Err(err) => panic!("{usage:?}: {err}"),
            };
            let levels: Vec<Vec<u8>> = reader.levels().map(|l| l.data.to_vec()).collect();
            assert_eq!(levels.len(), 6, "{usage:?}: 32 → 1 is six levels");
            let block_4x4 = &levels[3];
            for (i, level) in levels.iter().enumerate().skip(4) {
                assert!(level.iter().any(|&b| b != 0), "{usage:?}: level {i} is all zero");
                assert_eq!(
                    level, block_4x4,
                    "{usage:?}: level {i} differs from the 4×4 block"
                );
            }
        }
    }

    /// The Android cook of a packed height/curvature image keeps the BC5 `.rg` layout when asked
    /// for [`AstcContent::Rg`]: height in R, curvature in G, alpha 1. The usage's own
    /// (`Normal`) layout would put height in R, G and B and curvature in A.
    #[test]
    fn android_rg_content_keeps_the_rg_layout() {
        let rgba: Vec<u8> = [200u8, 60, 255, 255].repeat(16 * 16);
        let cooked = cook_texture_rgba8_content(
            16,
            16,
            &rgba,
            TextureCookUsage::NormalMap,
            "android",
            TextureCookQuality::High,
            AstcContent::Rg,
        )
        .expect("cook");
        let reader = ktx2::Reader::new(&cooked.ktx2[..]).expect("ktx2");
        let level0 = reader.levels().next().expect("level 0");
        let px =
            crate::astc_compress::decompress_astc_rgba8(16, 16, level0.data, (4, 4), false).expect("decode");
        let (r, g, b, a) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]), px[3]);
        assert!((r - 200).abs() <= 4, "height in R: {r}");
        assert!((g - 60).abs() <= 4, "curvature in G: {g}");
        assert!(b <= 4, "B is 0: {b}");
        assert_eq!(a, 255);
    }

    /// A smooth RGBA test image with alpha below 255 when `alpha`.
    fn smooth(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&[
                    (x * 255 / (w - 1)) as u8,
                    (y * 255 / (h - 1)) as u8,
                    (((x + y) * 255) / (w + h - 2)) as u8,
                    if alpha {
                        (x * 200 / (w - 1)) as u8 + 20
                    } else {
                        255
                    },
                ]);
            }
        }
        out
    }

    #[test]
    fn the_android_etc2_cook_picks_the_format_per_usage() {
        use ktx2::Format as F;
        for (usage, alpha, want) in [
            (TextureCookUsage::ColorSrgb, false, F::ETC2_R8G8B8_SRGB_BLOCK),
            (TextureCookUsage::ColorSrgb, true, F::ETC2_R8G8B8A8_SRGB_BLOCK),
            (TextureCookUsage::Ui, true, F::ETC2_R8G8B8A8_SRGB_BLOCK),
            (TextureCookUsage::ColorLinear, false, F::ETC2_R8G8B8_UNORM_BLOCK),
            (TextureCookUsage::NormalMap, false, F::EAC_R11G11_UNORM_BLOCK),
            (TextureCookUsage::Mask, false, F::EAC_R11_UNORM_BLOCK),
        ] {
            let rgba = smooth(16, 16, alpha);
            let cooked = cook_texture_rgba8_android(
                16,
                16,
                &rgba,
                usage,
                TextureCookQuality::High,
                AndroidTextureFormat::Etc2,
            )
            .expect("cook");
            let reader = ktx2::Reader::new(&cooked.ktx2[..]).expect("ktx2");
            assert_eq!(reader.header().format, Some(want), "{usage:?} alpha {alpha}");
            // 16 → 1 is five levels; the tail levels are one block each, never all zero.
            let levels: Vec<&[u8]> = reader.levels().map(|l| l.data).collect();
            assert_eq!(levels.len(), 5, "{usage:?}");
            assert!(
                levels[4].iter().any(|&b| b != 0),
                "{usage:?}: 1×1 level is all zero"
            );
        }
        assert!(
            cook_texture_rgba8_android(
                2,
                2,
                &[0; 16],
                TextureCookUsage::Mask,
                TextureCookQuality::High,
                AndroidTextureFormat::Etc2
            )
            .is_err()
        );
    }

    #[test]
    fn the_android_astc_format_is_the_platform_cook() {
        let rgba = smooth(16, 16, false);
        let via_format = cook_texture_rgba8_android(
            16,
            16,
            &rgba,
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
            AndroidTextureFormat::Astc,
        )
        .expect("cook");
        let via_platform = cook_texture_rgba8(
            16,
            16,
            &rgba,
            TextureCookUsage::ColorSrgb,
            "android",
            TextureCookQuality::High,
        )
        .expect("cook");
        assert_eq!(via_format.ktx2, via_platform.ktx2);
        assert_eq!(AndroidTextureFormat::default(), AndroidTextureFormat::Astc);
        for f in [AndroidTextureFormat::Astc, AndroidTextureFormat::Etc2] {
            assert_eq!(AndroidTextureFormat::from_label(f.label()), Some(f));
        }
        assert_eq!(AndroidTextureFormat::from_label("pvrtc"), None);
    }

    #[test]
    fn the_size_report_counts_whole_chains_and_measures_both_codecs() {
        let rgba = smooth(64, 64, false);
        let row = android_format_report(
            64,
            64,
            &rgba,
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
        )
        .expect("report");
        // 64 → 1 is seven levels: 256 + 64 + 16 + 4 + 1 + 1 + 1 = 343 blocks.
        assert_eq!(row.rgba8_bytes, (4096 + 1024 + 256 + 64 + 16 + 4 + 1) * 4);
        assert_eq!(row.astc_block, "4x4");
        assert_eq!(row.astc_bytes, 343 * 16);
        assert_eq!(row.etc2_format, "ETC2_R8G8B8_SRGB");
        assert_eq!(row.etc2_bytes, 343 * 8, "ETC2 RGB8 is half the size of ASTC 4x4");
        assert!(row.astc_psnr_db > 35.0, "ASTC {:.1} dB", row.astc_psnr_db);
        assert!(row.etc2_psnr_db > 30.0, "ETC2 {:.1} dB", row.etc2_psnr_db);

        let normals = android_format_report(
            32,
            32,
            &smooth(32, 32, false),
            TextureCookUsage::NormalMap,
            TextureCookQuality::High,
        )
        .expect("report");
        assert_eq!(normals.etc2_format, "EAC_R11G11_UNORM");
        assert_eq!(
            normals.etc2_bytes, normals.astc_bytes,
            "RG11 and ASTC 4x4 are both 8 bits a texel"
        );
        assert!(
            normals.etc2_psnr_db > 35.0,
            "EAC RG11 {:.1} dB",
            normals.etc2_psnr_db
        );

        let table = format_android_report(&[("a.png".into(), row), ("n.png".into(), normals)]);
        assert_eq!(table.lines().count(), 4, "{table}");
        assert!(
            table.lines().nth(1).is_some_and(|l| l.starts_with("a.png")),
            "{table}"
        );
        assert!(
            table.contains("ETC2_R8G8B8_SRGB") && table.contains("(2 textures)"),
            "{table}"
        );
    }
}
