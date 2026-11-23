// SPDX-License-Identifier: MPL-2.0
//! Block-compressed material maps: cooked KTX2 (BC1/BC3/BC4/BC5/BC7 on desktop, ASTC on
//! Android, ETC2/EAC when a pak carries them) uploaded as they are, instead of the RGBA8
//! [`crate::LabTexture`] path (4 bytes a texel; BC7 and ASTC 4×4 are 1).
//!
//! A [`LabMaterial`](crate::LabMaterial) may carry [`CompressedMaps`]. For each present slot:
//!
//! - the device can sample the format ([`wgpu::TextureFormat::required_features`]): the cooked
//!   mip chain is uploaded as it is, and the frame's texture streaming cap
//!   ([`crate::TextureStreaming::max_px`]) drops the top levels;
//! - it cannot, and the material has no RGBA8 map for the slot: the cooked chain is decoded to
//!   RGBA8 on the CPU ([`CompressedTexture::decode_rgba8`]: `regia_asset::bcn_decode` for BCn,
//!   `regia_asset::astc_decode` for ASTC LDR, `regia_asset::etc2_decode` for ETC2 / EAC; ADR-2063)
//!   and takes the RGBA8 path; this costs 4× the memory or more and is logged once. The desktop
//!   cook writes BCn and the Android cook ASTC (or ETC2), so this only happens when a pak is run
//!   on a GPU of the other family (an Android GPU without ASTC, an emulator or a desktop GPU
//!   without ASTC or ETC2); [`texture_path`] says which path a format takes on a device;
//! - the RGBA8 map (if any) is used.
//!
//! Colour slots (base colour, emissive) are sampled through an sRGB view and data slots through a
//! linear one whatever the cooked format says (ADR-038): the texture is created with the other
//! variant as a view format.
//!
//! Normal maps: BC5 / EAC RG11 store X and Y only and the Android cook's ASTC normals store X in
//! RGB and Y in A ([`NormalLayout`]); the forward shader rebuilds Z from the material's layout
//! code. `regia_asset_pipeline`'s cook and `cook_material_maps` write the files.

use std::sync::Arc;

/// How a texture in the normal slot stores the tangent-space normal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NormalLayout {
    /// X, Y, Z in R, G, B (RGBA8, BC7, BC1/BC3).
    #[default]
    Xyz,
    /// X, Y in R, G; Z rebuilt as `sqrt(1 − x² − y²)` (BC5, EAC RG11).
    Xy,
    /// X in R (and G, B), Y in A; Z rebuilt (ASTC normal mode, `RRRG` swizzle at encode).
    XInRgbYInA,
}

impl NormalLayout {
    /// The shader's layout code (bits 1-2 of `LabMaterial.alpha.z`, beside the unlit flag).
    #[must_use]
    pub fn shader_code(self) -> f32 {
        match self {
            Self::Xyz => 0.0,
            Self::Xy => 1.0,
            Self::XInRgbYInA => 2.0,
        }
    }
}

/// One cooked map: a block-compressed mip chain, level 0 first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressedTexture {
    /// Block-compressed format (sRGB variants for colour slots).
    pub format: wgpu::TextureFormat,
    /// Level-0 width in texels.
    pub width: u32,
    /// Level-0 height in texels.
    pub height: u32,
    /// Tightly packed block rows per level, level 0 first. Views of the KTX2 bytes they were read
    /// from (a cooked texture loaded from a pak points into the mapped pak), so cloning a texture
    /// or cutting levels never copies them; build one from a `Vec<u8>` with `.into()`.
    pub levels: Vec<regia_asset::PakPayload>,
    /// Channel layout when bound as a normal map.
    pub normal_layout: NormalLayout,
    /// What the cook knows about the chain, when it was cooked with mip streaming metadata.
    pub mip_stream: Option<regia_asset::MipStreamMeta>,
}

/// Errors from [`CompressedTexture::from_ktx2`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompressedTextureError {
    /// The container did not parse, a parsed image is wider or taller than 32768 texels, or
    /// the file lists more mip levels than that edge can have.
    #[error("ktx2: {0}")]
    Ktx2(String),
    /// Supercompressed, undefined or not a block-compressed format the renderer samples.
    #[error("unsupported texture format {0}")]
    Format(String),
    /// A level is shorter than its block rows.
    #[error("level {0} is truncated")]
    Truncated(u32),
}

/// Largest texture edge a KTX2 file may declare: beyond any GPU's limit, and small enough
/// that level byte counts and block rounding stay inside `u32`.
const MAX_EDGE: u32 = 32_768;

/// How many mip levels an image of this size can have: `floor(log2(max edge)) + 1`.
/// An 8×8 image has 4; a 1×1 image has 1. Never more than 16 under [`MAX_EDGE`].
fn mip_limit(width: u32, height: u32) -> u32 {
    let edge = width.max(height).max(1);
    32 - edge.leading_zeros()
}

/// The wgpu format of a KTX2 block-compressed format (2D LDR formats only).
fn block_format(format: ktx2::Format) -> Option<wgpu::TextureFormat> {
    use ktx2::Format as K;
    use wgpu::{AstcBlock as B, AstcChannel as C, TextureFormat as W};
    let astc = |block, srgb: bool| W::Astc {
        block,
        channel: if srgb { C::UnormSrgb } else { C::Unorm },
    };
    Some(match format {
        K::BC1_RGBA_UNORM_BLOCK | K::BC1_RGB_UNORM_BLOCK => W::Bc1RgbaUnorm,
        K::BC1_RGBA_SRGB_BLOCK | K::BC1_RGB_SRGB_BLOCK => W::Bc1RgbaUnormSrgb,
        K::BC3_UNORM_BLOCK => W::Bc3RgbaUnorm,
        K::BC3_SRGB_BLOCK => W::Bc3RgbaUnormSrgb,
        K::BC4_UNORM_BLOCK => W::Bc4RUnorm,
        K::BC5_UNORM_BLOCK => W::Bc5RgUnorm,
        K::BC7_UNORM_BLOCK => W::Bc7RgbaUnorm,
        K::BC7_SRGB_BLOCK => W::Bc7RgbaUnormSrgb,
        K::ETC2_R8G8B8_UNORM_BLOCK => W::Etc2Rgb8Unorm,
        K::ETC2_R8G8B8_SRGB_BLOCK => W::Etc2Rgb8UnormSrgb,
        K::ETC2_R8G8B8A1_UNORM_BLOCK => W::Etc2Rgb8A1Unorm,
        K::ETC2_R8G8B8A1_SRGB_BLOCK => W::Etc2Rgb8A1UnormSrgb,
        K::ETC2_R8G8B8A8_UNORM_BLOCK => W::Etc2Rgba8Unorm,
        K::ETC2_R8G8B8A8_SRGB_BLOCK => W::Etc2Rgba8UnormSrgb,
        K::EAC_R11_UNORM_BLOCK => W::EacR11Unorm,
        K::EAC_R11G11_UNORM_BLOCK => W::EacRg11Unorm,
        K::ASTC_4x4_UNORM_BLOCK => astc(B::B4x4, false),
        K::ASTC_4x4_SRGB_BLOCK => astc(B::B4x4, true),
        K::ASTC_5x4_UNORM_BLOCK => astc(B::B5x4, false),
        K::ASTC_5x4_SRGB_BLOCK => astc(B::B5x4, true),
        K::ASTC_5x5_UNORM_BLOCK => astc(B::B5x5, false),
        K::ASTC_5x5_SRGB_BLOCK => astc(B::B5x5, true),
        K::ASTC_6x5_UNORM_BLOCK => astc(B::B6x5, false),
        K::ASTC_6x5_SRGB_BLOCK => astc(B::B6x5, true),
        K::ASTC_6x6_UNORM_BLOCK => astc(B::B6x6, false),
        K::ASTC_6x6_SRGB_BLOCK => astc(B::B6x6, true),
        K::ASTC_8x5_UNORM_BLOCK => astc(B::B8x5, false),
        K::ASTC_8x5_SRGB_BLOCK => astc(B::B8x5, true),
        K::ASTC_8x6_UNORM_BLOCK => astc(B::B8x6, false),
        K::ASTC_8x6_SRGB_BLOCK => astc(B::B8x6, true),
        K::ASTC_8x8_UNORM_BLOCK => astc(B::B8x8, false),
        K::ASTC_8x8_SRGB_BLOCK => astc(B::B8x8, true),
        K::ASTC_10x5_UNORM_BLOCK => astc(B::B10x5, false),
        K::ASTC_10x5_SRGB_BLOCK => astc(B::B10x5, true),
        K::ASTC_10x6_UNORM_BLOCK => astc(B::B10x6, false),
        K::ASTC_10x6_SRGB_BLOCK => astc(B::B10x6, true),
        K::ASTC_10x8_UNORM_BLOCK => astc(B::B10x8, false),
        K::ASTC_10x8_SRGB_BLOCK => astc(B::B10x8, true),
        K::ASTC_10x10_UNORM_BLOCK => astc(B::B10x10, false),
        K::ASTC_10x10_SRGB_BLOCK => astc(B::B10x10, true),
        K::ASTC_12x10_UNORM_BLOCK => astc(B::B12x10, false),
        K::ASTC_12x10_SRGB_BLOCK => astc(B::B12x10, true),
        K::ASTC_12x12_UNORM_BLOCK => astc(B::B12x12, false),
        K::ASTC_12x12_SRGB_BLOCK => astc(B::B12x12, true),
        _ => return None,
    })
}

/// Bytes of one block row and the block-row count of a `w`×`h` level of `format`.
fn block_layout(format: wgpu::TextureFormat, w: u32, h: u32) -> (u32, u32) {
    let (bw, bh) = format.block_dimensions();
    let block_bytes = format.block_copy_size(None).unwrap_or(16);
    (w.div_ceil(bw) * block_bytes, h.div_ceil(bh))
}

/// Dimension of mip `level` of a `dim`-wide image: the shift saturates past the
/// chain's end (and never panics at 32+), `0` reading as one texel.
fn mip_dim(dim: u32, level: u32) -> u32 {
    dim.checked_shr(level).unwrap_or(0).max(1)
}

/// A CPU decoder for a block format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuDecoder {
    /// `regia_asset::bcn_decode`.
    Bcn(regia_asset::bcn_decode::BcnKind),
    /// `regia_asset::astc_decode` (LDR), with the footprint and the sRGB endpoint expansion.
    Astc((u32, u32), bool),
    /// `regia_asset::etc2_decode`.
    Etc2(regia_asset::etc2_decode::Etc2Format),
}

/// The CPU decoder for `format`, when there is one.
fn cpu_decoder(format: wgpu::TextureFormat) -> Option<CpuDecoder> {
    use regia_asset::bcn_decode::BcnKind as K;
    use regia_asset::etc2_decode::Etc2Format as E;
    use wgpu::TextureFormat as W;
    Some(match format {
        W::Bc1RgbaUnorm | W::Bc1RgbaUnormSrgb => CpuDecoder::Bcn(K::Bc1),
        W::Bc3RgbaUnorm | W::Bc3RgbaUnormSrgb => CpuDecoder::Bcn(K::Bc3),
        W::Bc4RUnorm => CpuDecoder::Bcn(K::Bc4),
        W::Bc5RgUnorm => CpuDecoder::Bcn(K::Bc5),
        W::Bc7RgbaUnorm | W::Bc7RgbaUnormSrgb => CpuDecoder::Bcn(K::Bc7),
        W::Astc { channel, .. } if channel != wgpu::AstcChannel::Hdr => {
            CpuDecoder::Astc(format.block_dimensions(), channel == wgpu::AstcChannel::UnormSrgb)
        }
        W::Etc2Rgb8Unorm | W::Etc2Rgb8UnormSrgb => CpuDecoder::Etc2(E::Rgb8),
        W::Etc2Rgb8A1Unorm | W::Etc2Rgb8A1UnormSrgb => CpuDecoder::Etc2(E::Rgb8A1),
        W::Etc2Rgba8Unorm | W::Etc2Rgba8UnormSrgb => CpuDecoder::Etc2(E::Rgba8),
        W::EacR11Unorm => CpuDecoder::Etc2(E::R11),
        W::EacRg11Unorm => CpuDecoder::Etc2(E::Rg11),
        _ => return None,
    })
}

/// How a device samples a cooked block format (ADR-2063).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TexturePath {
    /// The device samples the format: the blocks are uploaded as they are.
    Native,
    /// The device cannot: the chain is decoded to RGBA8 on the CPU (4 bytes a texel).
    CpuDecode,
    /// The device cannot and there is no CPU decoder (HDR ASTC, BC6H, signed EAC): the slot is
    /// unbound.
    Unbound,
}

impl TexturePath {
    /// `native`, `cpu_decode` or `unbound` (the agent protocol's names).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::CpuDecode => "cpu_decode",
            Self::Unbound => "unbound",
        }
    }
}

/// The block format families a cook writes, by the agent protocol's name, with the format each
/// is probed with: `bc` (the desktop cook), `astc` and `etc2` (the Android cook's two families).
pub const FAMILY_PROBES: [(&str, wgpu::TextureFormat); 3] = [
    ("bc", wgpu::TextureFormat::Bc7RgbaUnormSrgb),
    (
        "astc",
        wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::UnormSrgb,
        },
    ),
    ("etc2", wgpu::TextureFormat::Etc2Rgb8UnormSrgb),
];

/// The [`TexturePath`] of each family in [`FAMILY_PROBES`] on a device with `features`
/// (`render.get_capabilities` `texture_paths`).
#[must_use]
pub fn family_paths(features: wgpu::Features) -> [(&'static str, TexturePath); 3] {
    FAMILY_PROBES.map(|(name, format)| (name, texture_path(format, features)))
}

/// The path a texture of `format` takes on a device with `features`: uploaded as it is when the
/// device samples it, else decoded on the CPU when a decoder exists, else unbound.
#[must_use]
pub fn texture_path(format: wgpu::TextureFormat, features: wgpu::Features) -> TexturePath {
    if features.contains(format.required_features()) {
        TexturePath::Native
    } else if cpu_decoder(format).is_some() {
        TexturePath::CpuDecode
    } else {
        TexturePath::Unbound
    }
}

/// The RGBA8 format a chain of block `format` decodes to on a device that cannot sample it (sRGB
/// stays sRGB), or `None` when there is no CPU decoder ([`TexturePath::Unbound`]).
#[must_use]
pub fn decoded_rgba8_format(format: wgpu::TextureFormat) -> Option<wgpu::TextureFormat> {
    cpu_decoder(format)?;
    Some(if format.is_srgb() {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    })
}

/// One `width`×`height` level of block `format` decoded to row-major RGBA8: the values the GPU
/// would sample, with no channel rewritten (BC4 / EAC R11 as `(r, 0, 0, 1)`, BC5 / EAC RG11 as
/// `(r, g, 0, 1)`). For hosts with their own upload path; a material map goes through
/// [`CompressedTexture::decode_level_rgba8`].
///
/// # Errors
///
/// `format` has no CPU decoder, or `data` is shorter than the level's blocks.
pub fn decode_blocks_rgba8(
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    match cpu_decoder(format).ok_or_else(|| format!("no CPU decoder for {format:?}"))? {
        CpuDecoder::Bcn(kind) => {
            regia_asset::bcn_decode::decode_bcn(kind, width, height, data).map_err(|e| e.to_string())
        }
        CpuDecoder::Astc(block, srgb) => {
            regia_asset::astc_decode::decode_astc(width, height, block, srgb, data).map_err(|e| e.to_string())
        }
        CpuDecoder::Etc2(kind) => {
            regia_asset::etc2_decode::decode_etc2(kind, width, height, data).map_err(|e| e.to_string())
        }
    }
}

impl CompressedTexture {
    /// Reads a KTX2 file (block-compressed 2D, not supercompressed). BC5 and EAC RG11 are
    /// two-channel normals ([`NormalLayout::Xy`]); everything else reads as XYZ.
    ///
    /// # Errors
    ///
    /// The container does not parse or carries a hostile data format descriptor, a parsed image
    /// edge is above 32768 texels, or the file lists more mip levels than that edge can have (all
    /// [`CompressedTextureError::Ktx2`]), the format is not block-compressed, or a level is
    /// truncated.
    pub fn from_ktx2(bytes: &[u8]) -> Result<Self, CompressedTextureError> {
        Self::from_ktx2_shared(&regia_asset::PakPayload::from(bytes))
    }

    /// [`Self::from_ktx2`] without copying: every level is a view of `bytes` (for a cooked
    /// texture loaded from a pak, of the mapped pak itself).
    ///
    /// # Errors
    ///
    /// As [`Self::from_ktx2`], including an image edge above 32768 texels or a mip chain
    /// longer than the edge allows.
    pub fn from_ktx2_shared(bytes: &regia_asset::PakPayload) -> Result<Self, CompressedTextureError> {
        // The ktx2 parser panics on a hostile DFD (an abort under panic = "abort"); a caller with
        // raw bytes has not been through `CookedTexture::validate` (ADR-5869).
        if let Some(reason) = regia_asset::hostile_ktx2_dfd(&bytes[..]) {
            return Err(CompressedTextureError::Ktx2(reason.into()));
        }
        let reader =
            ktx2::Reader::new(&bytes[..]).map_err(|e| CompressedTextureError::Ktx2(e.to_string()))?;
        let header = reader.header();
        if header.supercompression_scheme.is_some() {
            return Err(CompressedTextureError::Format("supercompressed".into()));
        }
        let ktx_format = header
            .format
            .ok_or_else(|| CompressedTextureError::Format("undefined".into()))?;
        let format = block_format(ktx_format)
            .ok_or_else(|| CompressedTextureError::Format(format!("{ktx_format:?}")))?;
        let (width, height) = (header.pixel_width.max(1), header.pixel_height.max(1));
        // Bounds every `u32` size and block-rounding product below (and the upload's).
        if width > MAX_EDGE || height > MAX_EDGE {
            return Err(CompressedTextureError::Ktx2(format!(
                "{width}×{height} exceeds the {MAX_EDGE} texel edge limit"
            )));
        }
        // Only 2D images load: a cubemap, array or 3D file interleaves faces,
        // layers and z slices into each level, and reading one as a 2D mip would
        // silently keep face 0 / layer 0 / slice 0.
        if header.face_count != 1 || header.layer_count > 1 || header.pixel_depth > 1 {
            return Err(CompressedTextureError::Format(
                "not a 2D image (cubemap, array or 3D KTX2)".into(),
            ));
        }
        // `Reader` accepts any level count whose index table fits the file; a
        // chain longer than the image's own mip chain shifts a u32 dimension by
        // 32+ (a debug panic) and would overflow the upload's mip count.
        let level_count = header.level_count.max(1);
        let max_levels = mip_limit(width, height);
        if level_count > max_levels {
            return Err(CompressedTextureError::Ktx2(format!(
                "{level_count} mip levels for a {width}×{height} image (at most {max_levels})"
            )));
        }
        let mut levels = Vec::new();
        for (i, level) in reader.levels().enumerate() {
            let (w, h) = (mip_dim(width, i as u32), mip_dim(height, i as u32));
            let (row, rows) = block_layout(format, w, h);
            let need = (row * rows) as usize;
            let data = level
                .data
                .get(..need)
                .and_then(|data| bytes.subslice(data))
                .ok_or(CompressedTextureError::Truncated(i as u32))?;
            levels.push(data);
        }
        if levels.is_empty() {
            return Err(CompressedTextureError::Truncated(0));
        }
        let mip_stream = regia_asset::MipStreamMeta::from_reader(&reader);
        let normal_layout = if matches!(
            format,
            wgpu::TextureFormat::Bc5RgUnorm | wgpu::TextureFormat::EacRg11Unorm
        ) {
            NormalLayout::Xy
        } else {
            NormalLayout::Xyz
        };
        Ok(Self {
            format,
            width,
            height,
            levels,
            normal_layout,
            mip_stream,
        })
    }

    /// Reads a cooked texture (`regia cook` output). The usage says how a normal map was
    /// encoded: the Android cook's ASTC normals are [`NormalLayout::XInRgbYInA`].
    ///
    /// # Errors
    ///
    /// As [`Self::from_ktx2`].
    pub fn from_cooked(texture: &regia_asset::CookedTexture) -> Result<Self, CompressedTextureError> {
        let mut t = Self::from_ktx2_shared(&texture.ktx2)?;
        if texture.usage == regia_asset::TextureCookUsage::NormalMap
            && matches!(t.format, wgpu::TextureFormat::Astc { .. })
        {
            t.normal_layout = NormalLayout::XInRgbYInA;
        }
        Ok(t)
    }

    /// GPU bytes of the chain from level `skip` down.
    #[must_use]
    pub fn bytes_from(&self, skip: u32) -> u64 {
        self.levels
            .iter()
            .skip(skip as usize)
            .map(|l| l.len() as u64)
            .sum()
    }

    /// Whether `features` let the device sample this format.
    #[must_use]
    pub fn supported(&self, features: wgpu::Features) -> bool {
        features.contains(self.format.required_features())
    }

    /// Level 0 decoded to row-major RGBA8 ([`Self::decode_level_rgba8`]).
    #[must_use]
    pub fn decode_rgba8(&self) -> Option<crate::LabTexture> {
        self.decode_level_rgba8(0)
    }

    /// Every level decoded to RGBA8, level 0 first: the chain the GPU would sample, for devices
    /// that cannot sample the format. `None` if any level does not decode.
    #[must_use]
    pub fn decode_chain_rgba8(&self) -> Option<Vec<crate::LabTexture>> {
        let _span = tracing::info_span!("compressed_texture_decode").entered();
        if cpu_decoder(self.format).is_some() {
            regia_asset::cooked_pack::note_texture_cpu_decode();
        }
        (0..self.levels.len() as u32)
            .map(|level| self.decode_level_rgba8(level))
            .collect()
    }

    /// Level `level` decoded to row-major RGBA8 (the bytes the GPU would sample, so sRGB-encoded
    /// for an sRGB format). A two-channel normal map gets its Z rebuilt into B, and an ASTC
    /// normal-mode map (`X X X Y`) its Y moved to G and Z rebuilt, so either binds as an ordinary
    /// XYZ normal map. `None` for formats without a CPU decoder (HDR ASTC, BC6H, signed EAC), a
    /// missing level or a truncated one.
    #[must_use]
    pub fn decode_level_rgba8(&self, level: u32) -> Option<crate::LabTexture> {
        let data = self.levels.get(level as usize)?;
        let (width, height) = (mip_dim(self.width, level), mip_dim(self.height, level));
        let mut rgba = decode_blocks_rgba8(self.format, width, height, data).ok()?;
        let rebuild_z = |px: &mut [u8; 4]| {
            let x = f32::from(px[0]) / 127.5 - 1.0;
            let y = f32::from(px[1]) / 127.5 - 1.0;
            let z = (1.0 - x * x - y * y).max(0.0).sqrt();
            px[2] = ((z * 0.5 + 0.5) * 255.0).round() as u8;
        };
        match self.normal_layout {
            NormalLayout::Xy => rgba.as_chunks_mut::<4>().0.iter_mut().for_each(rebuild_z),
            NormalLayout::XInRgbYInA => {
                for px in rgba.as_chunks_mut::<4>().0 {
                    px[1] = px[3];
                    px[3] = 255;
                    rebuild_z(px);
                }
            }
            NormalLayout::Xyz => {}
        }
        Some(crate::LabTexture { width, height, rgba })
    }

    /// Uploads the chain without its top `skip` levels (one level always stays), sampled
    /// through an sRGB view when `srgb` (colour slots) and a linear one otherwise.
    pub(crate) fn upload(
        &self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        skip: u32,
        srgb: bool,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        self.upload_views(d, q, skip, srgb, false)
    }

    /// [`Self::upload`], the texture viewable in both color spaces when `siblings` and the
    /// format has an sRGB variant (a counted texture binds either, ADR-8610).
    pub(crate) fn upload_views(
        &self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        skip: u32,
        srgb: bool,
        siblings: bool,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let _span = tracing::info_span!("compressed_texture_upload").entered();
        let (bw, bh) = self.format.block_dimensions();
        let limit = mip_limit(self.width, self.height);
        let level_count = self.levels.len() as u32;
        let mut skip = skip
            .min(level_count.saturating_sub(1))
            .min(limit.saturating_sub(1));
        // Level 0 of a block texture spans whole blocks: never skip below one block.
        while skip > 0 && (mip_dim(self.width, skip) < bw || mip_dim(self.height, skip) < bh) {
            skip -= 1;
        }
        let (width, height) = (mip_dim(self.width, skip), mip_dim(self.height, skip));
        let mip_level_count = level_count.saturating_sub(skip).min(limit - skip).max(1);
        let view_format = if srgb {
            self.format.add_srgb_suffix()
        } else {
            self.format.remove_srgb_suffix()
        };
        // The view format when it is not the texture's own; with `siblings`, the format's other
        // color-space variant as well (at most one of the two differs from `self.format`).
        let other = match crate::frame::sibling_view_format(self.format, siblings) {
            Some(sibling) => [sibling],
            None => [view_format],
        };
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("material texture (bcn)"),
            // Block formats need whole blocks at level 0.
            size: wgpu::Extent3d {
                width: width.next_multiple_of(bw),
                height: height.next_multiple_of(bh),
                depth_or_array_layers: 1,
            },
            mip_level_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: if other[0] == self.format { &[] } else { &other },
        });
        for (mip, data) in self
            .levels
            .iter()
            .skip(skip as usize)
            .take(mip_level_count as usize)
            .enumerate()
        {
            let mip = mip as u32;
            let (w, h) = (mip_dim(width, mip), mip_dim(height, mip));
            let (row, rows) = block_layout(self.format, w, h);
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: mip,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(rows),
                },
                // The copy extent is the physical (whole-block) size of the level.
                wgpu::Extent3d {
                    width: w.div_ceil(bw) * bw,
                    height: rows * bh,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(view_format),
            ..Default::default()
        });
        (texture, view)
    }
}

/// Pre-compressed maps of a material, by slot (any may be absent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompressedMaps {
    /// Base colour (sampled sRGB).
    pub albedo: Option<CompressedTexture>,
    /// Tangent-space normal (linear; [`CompressedTexture::normal_layout`]).
    pub normal: Option<CompressedTexture>,
    /// Occlusion or packed ORM (linear).
    pub occlusion: Option<CompressedTexture>,
    /// Emissive (sampled sRGB).
    pub emissive: Option<CompressedTexture>,
    /// Packed detail albedo (RGB) and roughness (A). Linear BC7: the shader reads both, and
    /// BC5 would drop the alpha. Not a streamed slot.
    pub detail: Option<Arc<CompressedTexture>>,
    /// Packed detail normal (RGB) and mask (A). Linear BC7. The forward shader reads XYZ and
    /// the mask; it does not rebuild Z, so this is not a BC5 normal.
    pub detail_normal: Option<Arc<CompressedTexture>>,
    /// Packed height (R) and curvature (G). BC5 (the shader reads R and G; alpha samples as 1).
    pub aux: Option<Arc<CompressedTexture>>,
}

impl CompressedMaps {
    /// Each chain without the levels above the active tier's resident cap
    /// ([`crate::texture_budget::resident_cap`]).
    #[must_use]
    pub fn fit_to_resident_cap(self) -> Self {
        let cap = crate::texture_budget::resident_cap();
        if cap == 0 {
            return self;
        }
        let fit = |t: Option<CompressedTexture>| t.map(|t| crate::texture_budget::fit_compressed(t, cap));
        Self {
            albedo: fit(self.albedo),
            normal: fit(self.normal),
            occlusion: fit(self.occlusion),
            emissive: fit(self.emissive),
            detail: fit_shared(self.detail, cap),
            detail_normal: fit_shared(self.detail_normal, cap),
            aux: fit_shared(self.aux, cap),
        }
    }

    /// The slots in [`crate::texture_stream::STREAMED_SLOTS`] order.
    pub(crate) fn slots(&self) -> [Option<&CompressedTexture>; 4] {
        [
            self.albedo.as_ref(),
            self.normal.as_ref(),
            self.occlusion.as_ref(),
            self.emissive.as_ref(),
        ]
    }

    /// GPU bytes of every present slot's full chain, including the packed detail and aux slots.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        let streamed: u64 = self.slots().iter().flatten().map(|t| t.bytes_from(0)).sum();
        let packed: u64 = [&self.detail, &self.detail_normal, &self.aux]
            .into_iter()
            .flatten()
            .map(|t| t.bytes_from(0))
            .sum();
        streamed + packed
    }
}

/// Drop levels above `cap`, keeping the same [`Arc`] when the chain already fits.
fn fit_shared(texture: Option<Arc<CompressedTexture>>, cap: u32) -> Option<Arc<CompressedTexture>> {
    let texture = texture?;
    if cap == 0 || compressed_fits(&texture, cap) {
        return Some(texture);
    }
    let owned = match Arc::try_unwrap(texture) {
        Ok(owned) => owned,
        Err(shared) => (*shared).clone(),
    };
    Some(Arc::new(crate::texture_budget::fit_compressed(owned, cap)))
}

/// `true` when [`crate::texture_budget::fit_compressed`] would not drop a level.
fn compressed_fits(texture: &CompressedTexture, cap: u32) -> bool {
    let (bw, bh) = texture.format.block_dimensions();
    let would_drop = texture.levels.len() > 1
        && texture.width.max(texture.height) > cap
        && (texture.width >> 1).is_multiple_of(bw)
        && (texture.height >> 1).is_multiple_of(bh)
        && (texture.width >> 1) > 0
        && (texture.height >> 1) > 0;
    !would_drop
}

/// Whether slot `slot` (in [`CompressedMaps::slots`] order) is sampled as sRGB.
pub(crate) const fn slot_is_srgb(slot: usize) -> bool {
    slot == 0 || slot == 3
}

/// The compressed slot `slot` of `maps` when the device can sample its format.
pub(crate) fn usable(
    maps: Option<&Arc<CompressedMaps>>,
    slot: usize,
    features: wgpu::Features,
) -> Option<&CompressedTexture> {
    maps.and_then(|m| m.slots()[slot])
        .filter(|t| t.supported(features))
}

/// [`CompressedMaps::slots`] names, for diagnostics.
const SLOT_NAMES: [&str; 4] = ["base_color", "normal", "orm", "emissive"];

/// The RGBA8 stand-in for compressed slot `slot` on a device that cannot sample its format,
/// when the material has no RGBA8 map there (`have_rgba`): every cooked level decoded, level 0
/// first, so it samples the cook's own mips. Reported as a texture cooked for another platform
/// ([`crate::texture_budget::report_wrong_platform_texture`]: warns once per format).
pub(crate) fn fallback(
    maps: Option<&Arc<CompressedMaps>>,
    slot: usize,
    features: wgpu::Features,
    have_rgba: bool,
) -> Option<Vec<crate::LabTexture>> {
    let t = maps.and_then(|m| m.slots()[slot])?;
    if have_rgba || t.supported(features) {
        return None;
    }
    let decoded = t.decode_chain_rgba8();
    let label = maps
        .and_then(crate::texture_budget::name_of_compressed)
        .map_or_else(
            || format!("material {}", SLOT_NAMES[slot % SLOT_NAMES.len()]),
            |name| format!("{name} {}", SLOT_NAMES[slot % SLOT_NAMES.len()]),
        );
    // The maps' address alone can be reused by a later material once these are dropped: the
    // format and size make a different texture at a reused address a new key.
    let key = format!("{:?}{}x{}", t.format, t.width, t.height)
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
        ^ maps.map_or(0, |m| Arc::as_ptr(m) as u64)
        ^ ((slot as u64) << 56);
    let bytes = decoded
        .as_ref()
        .map(|levels| levels.iter().map(|l| l.rgba.len() as u64).sum());
    crate::texture_budget::report_wrong_platform_texture(key, &label, t.format, bytes);
    decoded
}

/// The normal-slot layout the shader must use for `maps` on a device with `features`
/// (a decoded fallback is always XYZ).
pub(crate) fn normal_layout(maps: Option<&Arc<CompressedMaps>>, features: wgpu::Features) -> NormalLayout {
    usable(maps, 1, features).map_or(NormalLayout::Xyz, |t| t.normal_layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_rows_round_up() {
        use wgpu::TextureFormat as W;
        assert_eq!(block_layout(W::Bc7RgbaUnorm, 4096, 4096), (16384, 1024));
        assert_eq!(block_layout(W::Bc7RgbaUnorm, 2, 1), (16, 1));
        assert_eq!(block_layout(W::Bc1RgbaUnorm, 6, 5), (16, 2));
        let astc8 = W::Astc {
            block: wgpu::AstcBlock::B8x8,
            channel: wgpu::AstcChannel::Unorm,
        };
        assert_eq!(block_layout(astc8, 1024, 8), (2048, 1));
    }

    /// A BC7 KTX2 file with the given geometry: `level_count` levels of
    /// `level_bytes` each (sequential, largest first), one DFD, no supercompression.
    fn bc7_ktx2_with(
        pixel_width: u32,
        pixel_height: u32,
        face_count: u32,
        layer_count: u32,
        pixel_depth: u32,
        level_count: u32,
        level_bytes: u64,
    ) -> Vec<u8> {
        use ktx2::dfd::{Basic, Block};
        use ktx2::{Format, Header, Index, LevelIndex};
        let (basic, type_size) = Basic::from_format(Format::BC7_UNORM_BLOCK).expect("dfd");
        let dfd_body = Block::Basic(basic).to_vec();
        let dfd_total_len = 4u32 + dfd_body.len() as u32;
        let levels = level_count.max(1) as usize;
        let dfd_offset = Header::LENGTH + levels * LevelIndex::LENGTH;
        let data_offset = dfd_offset + dfd_total_len as usize;
        let index: Vec<LevelIndex> = (0..levels)
            .map(|i| LevelIndex {
                byte_offset: data_offset as u64 + i as u64 * level_bytes,
                byte_length: level_bytes,
                uncompressed_byte_length: level_bytes,
            })
            .collect();
        let header = Header {
            format: Some(Format::BC7_UNORM_BLOCK),
            type_size,
            pixel_width,
            pixel_height,
            pixel_depth,
            layer_count,
            face_count,
            level_count,
            supercompression_scheme: None,
            index: Index {
                dfd_byte_offset: dfd_offset as u32,
                dfd_byte_length: dfd_total_len,
                kvd_byte_offset: data_offset as u32,
                kvd_byte_length: 0,
                sgd_byte_offset: 0,
                sgd_byte_length: 0,
            },
        };
        let mut out = Vec::new();
        out.extend_from_slice(&header.as_bytes());
        for level in &index {
            out.extend_from_slice(&level.as_bytes());
        }
        out.extend_from_slice(&dfd_total_len.to_le_bytes());
        out.extend_from_slice(&dfd_body);
        out.extend_from_slice(&vec![0u8; levels * level_bytes as usize]);
        out
    }

    /// A one-level 8×8 BC7 KTX2 file (four blocks, bytes 0..64).
    fn bc7_ktx2() -> Vec<u8> {
        let mut file = bc7_ktx2_with(8, 8, 1, 0, 0, 1, 64);
        let data_offset = file.len() - 64;
        file[data_offset..].copy_from_slice(&(0..64u8).collect::<Vec<u8>>());
        file
    }

    #[test]
    fn a_mip_chain_longer_than_the_image_is_rejected() {
        // `ktx2::Reader` accepts any level count whose index table and byte
        // ranges fit the file; the loader must reject a chain longer than the
        // image's own mip chain instead of `width >> 32` (a debug panic).
        let file = bc7_ktx2_with(4, 4, 1, 0, 0, 33, 16);
        // A malformed container (`Ktx2`), like an edge past the limit (ADR-5674 unifies the
        // two lanes' fixes on that variant).
        assert!(matches!(
            CompressedTexture::from_ktx2(&file),
            Err(CompressedTextureError::Ktx2(_))
        ));
    }

    #[test]
    fn a_full_mip_chain_still_loads() {
        // 4×4, 2×2, 1×1 BC7: three one-block levels.
        let file = bc7_ktx2_with(4, 4, 1, 0, 0, 3, 16);
        let t = CompressedTexture::from_ktx2(&file).expect("chain");
        assert_eq!((t.width, t.height, t.levels.len()), (4, 4, 3));
        assert_eq!(t.decode_level_rgba8(2).expect("level 2").rgba.len(), 4);
        // Past the chain the level is missing, and the checked shift must not panic.
        assert!(t.decode_level_rgba8(3).is_none());
        assert!(t.decode_level_rgba8(32).is_none());
    }

    #[test]
    fn a_cubemap_array_or_3d_file_is_rejected_not_loaded_as_face_zero() {
        // The level layout interleaves layer, face and z slice: read as a 2D
        // mip these would silently keep face 0 / layer 0 / slice 0.
        for (face_count, layer_count, pixel_depth, level_bytes, what) in [
            (6u32, 0u32, 0u32, 64 * 6, "cubemap"),
            (1, 4, 0, 64 * 4, "array"),
            (1, 0, 4, 64 * 4, "3d"),
        ] {
            let file = bc7_ktx2_with(8, 8, face_count, layer_count, pixel_depth, 1, level_bytes);
            assert!(
                matches!(
                    CompressedTexture::from_ktx2(&file),
                    Err(CompressedTextureError::Format(_))
                ),
                "{what} must be rejected"
            );
        }
    }

    /// An 8×8 BC7 KTX2 file with `levels` mip entries (audfix2-render-0-g; built on
    /// `bc7_ktx2_with`, 64 bytes per entry, so every entry is in range).
    ///
    /// A real 8×8 chain has four levels. Callers pass a longer count to build a file the
    /// container parser accepts and the loader must refuse.
    fn bc7_ktx2_levels(levels: u32) -> Vec<u8> {
        bc7_ktx2_with(8, 8, 1, 0, 0, levels.max(1), 64)
    }

    /// A chain longer than `floor(log2(edge)) + 1` is a corrupt cook. Rust panics on
    /// `u32 >> 32`, and wgpu rejects a mip count past that limit, so the loader must
    /// return an error instead of storing the chain.
    #[test]
    fn a_mip_chain_longer_than_the_shift_width_is_rejected() {
        let refuse = |levels: u32| match CompressedTexture::from_ktx2(&bc7_ktx2_levels(levels)) {
            Err(CompressedTextureError::Ktx2(message)) => {
                assert!(
                    message.contains("mip levels"),
                    "a {levels}-level 8×8 chain must name the mip limit, got {message}"
                );
            }
            other => panic!("a {levels}-level 8×8 chain is a Ktx2 error, got {other:?}"),
        };
        // Forty levels walks past a 32-bit shift (`width >> 32` panics).
        refuse(40);
        // Five levels fit in the shift but not in an 8×8 image (four mips: 8, 4, 2, 1).
        refuse(5);
        let legal = CompressedTexture::from_ktx2(&bc7_ktx2_levels(4)).expect("four mips of 8×8");
        assert_eq!((legal.width, legal.height, legal.levels.len()), (8, 8, 4));
    }

    #[test]
    fn an_absurd_edge_is_rejected_instead_of_wrapping() {
        let mut file = bc7_ktx2();
        // `pixelWidth` is the u32 at byte 20 of the KTX2 header.
        file[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            CompressedTexture::from_ktx2(&file),
            Err(CompressedTextureError::Ktx2(_))
        ));
        file[20..24].copy_from_slice(&32_769u32.to_le_bytes());
        match CompressedTexture::from_ktx2(&file) {
            Err(CompressedTextureError::Ktx2(message)) => {
                assert!(message.contains("32768"), "{message}");
            }
            other => panic!("an edge of 32769 is a Ktx2 error, got {other:?}"),
        }
        // A file too short to parse stays the parse path, not the edge-limit message.
        match CompressedTexture::from_ktx2(&file[..20]) {
            Err(CompressedTextureError::Ktx2(message)) => {
                assert!(!message.contains("32768"), "{message}");
            }
            other => panic!("a short file is a parse error, got {other:?}"),
        }
    }

    #[test]
    fn shared_levels_are_views_of_the_ktx2_bytes() {
        let file = regia_asset::PakPayload::from(bc7_ktx2());
        let t = CompressedTexture::from_ktx2_shared(&file).expect("bc7 ktx2");
        assert_eq!((t.width, t.height, t.levels.len()), (8, 8, 1));
        assert_eq!(t.levels[0], (0..64u8).collect::<Vec<u8>>());
        assert!(
            file.as_ptr_range().contains(&t.levels[0].as_ptr()),
            "level 0 is a view of the file, not a copy"
        );
        // Cloning the texture shares its levels.
        let copy = t.clone();
        assert_eq!(copy.levels[0].as_ptr(), t.levels[0].as_ptr());
        // The copying entry point reads the same chain.
        assert_eq!(CompressedTexture::from_ktx2(&file).expect("copy"), t);
        // A truncated level is still an error.
        let short = file.slice(0..file.len() - 1).expect("slice");
        assert!(CompressedTexture::from_ktx2_shared(&short).is_err());
    }

    /// sec-review-assets-agent (ADR-5869) finding 4: the raw-KTX2 entry points went straight to
    /// `ktx2::Reader::new`, which panics on a DFD texel-block dimension of 0xFF (an abort under
    /// panic = "abort"). They now refuse it as an error.
    #[test]
    fn a_hostile_dfd_is_an_error_not_a_panic() {
        let mut file = bc7_ktx2();
        let dfd_body = ktx2::Header::LENGTH + ktx2::LevelIndex::LENGTH + 4;
        file[dfd_body + ktx2::dfd::BlockHeader::LENGTH + 4] = 0xFF;
        match CompressedTexture::from_ktx2(&file) {
            Err(CompressedTextureError::Ktx2(msg)) => assert!(msg.contains("texel-block dimension"), "{msg}"),
            other => panic!("expected the DFD guard's error, got {other:?}"),
        }
        assert!(CompressedTexture::from_ktx2_shared(&regia_asset::PakPayload::from(file)).is_err());
    }

    /// The v0.5.0 input-path review's soak (ADR-5869): `REGIA_FUZZ_SOAK_ITERATIONS` (default
    /// 20 000) mutations each of a BC7 KTX2 through `CompressedTexture::from_ktx2` and of a colour
    /// LUT strip PNG through `ColorLut::from_png_strip`. Ignored in the gate;
    /// `cargo test -p regia_render --lib -- --ignored ktx2_and_lut_soak --nocapture`.
    #[test]
    #[ignore = "long soak; run by hand"]
    fn ktx2_and_lut_soak() {
        let iterations = std::env::var("REGIA_FUZZ_SOAK_ITERATIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(20_000);
        let seed = std::env::var("REGIA_FUZZ_SOAK_FIRST_SEED")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let ktx2 = bc7_ktx2();
        let errors = regia_test_utils::fuzz::fuzz_loader(&ktx2, seed, iterations, |bytes| {
            CompressedTexture::from_ktx2(bytes).map(|t| t.levels.len())
        });
        eprintln!("soak ktx2 iterations {iterations} errors {errors}");
        let mut strip = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut strip, 4, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(&[200_u8; 4 * 2 * 4]).expect("png data");
        }
        assert!(
            crate::lut::ColorLut::from_png_strip(&strip).is_ok(),
            "the fixture strip loads"
        );
        let errors = regia_test_utils::fuzz::fuzz_loader(&strip, seed, iterations, |bytes| {
            crate::lut::ColorLut::from_png_strip(bytes).map(|_| ())
        });
        eprintln!("soak lut.png iterations {iterations} errors {errors}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            CompressedTexture::from_ktx2(b"not a ktx2 file"),
            Err(CompressedTextureError::Ktx2(_))
        ));
    }

    fn solid(format: wgpu::TextureFormat, levels: Vec<Vec<u8>>) -> CompressedTexture {
        CompressedTexture {
            format,
            width: 8,
            height: 8,
            levels: levels.into_iter().map(Into::into).collect(),
            normal_layout: NormalLayout::Xyz,
            mip_stream: None,
        }
    }

    #[test]
    fn bytes_from_skips_top_levels() {
        let t = solid(
            wgpu::TextureFormat::Bc7RgbaUnorm,
            vec![vec![0; 64], vec![0; 16], vec![0; 16], vec![0; 16]],
        );
        assert_eq!(t.bytes_from(0), 112);
        assert_eq!(t.bytes_from(1), 48);
    }

    #[test]
    fn support_follows_the_format_feature() {
        let t = solid(wgpu::TextureFormat::Bc1RgbaUnorm, vec![vec![0; 32]]);
        assert!(t.supported(wgpu::Features::TEXTURE_COMPRESSION_BC));
        assert!(!t.supported(wgpu::Features::TEXTURE_COMPRESSION_ASTC));
    }

    #[test]
    fn decoded_bc5_normal_gets_its_z_back() {
        // BC5 block with both channels at 128 (r0 = r1 = 128): a flat normal.
        let block = [128u8, 128, 0, 0, 0, 0, 0, 0, 128, 128, 0, 0, 0, 0, 0, 0];
        let t = CompressedTexture {
            format: wgpu::TextureFormat::Bc5RgUnorm,
            width: 4,
            height: 4,
            levels: vec![block.to_vec().into()],
            normal_layout: NormalLayout::Xy,
            mip_stream: None,
        };
        let decoded = t.decode_rgba8().expect("bc5 decodes");
        assert_eq!(&decoded.rgba[..4], &[128, 128, 255, 255]);
    }

    #[test]
    fn fallback_only_without_support_and_without_rgba() {
        let maps = Arc::new(CompressedMaps {
            albedo: Some(solid(wgpu::TextureFormat::Bc1RgbaUnormSrgb, vec![vec![0; 32]])),
            ..CompressedMaps::default()
        });
        let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
        assert!(usable(Some(&maps), 0, bc).is_some());
        assert!(fallback(Some(&maps), 0, bc, false).is_none());
        assert!(fallback(Some(&maps), 0, wgpu::Features::empty(), true).is_none());
        let decoded = fallback(Some(&maps), 0, wgpu::Features::empty(), false).expect("decoded");
        assert_eq!((decoded[0].width, decoded[0].height), (8, 8));
        // The decode is reported as a texture cooked for another platform (render.problems).
        let wrong = crate::texture_budget::wrong_platform_textures();
        assert!(wrong.decoded >= 1, "{wrong:?}");
        assert!(wrong.formats.iter().any(|f| f == "Bc1RgbaUnormSrgb"), "{wrong:?}");
    }

    /// An Android cook's ASTC maps are used as they are on an ASTC device (no decode, no
    /// report); on a device without ASTC they are decoded on the CPU (ADR-2063) and reported as
    /// a texture cooked for another platform.
    #[test]
    fn astc_maps_are_used_directly_where_supported_and_decoded_elsewhere() {
        let astc = wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::UnormSrgb,
        };
        let maps = Arc::new(CompressedMaps {
            albedo: Some(solid(astc, vec![void_extent_astc([90, 140, 60, 255]).repeat(4)])),
            ..CompressedMaps::default()
        });
        let phone = wgpu::Features::TEXTURE_COMPRESSION_ASTC | wgpu::Features::TEXTURE_COMPRESSION_ETC2;
        assert!(usable(Some(&maps), 0, phone).is_some());
        assert!(fallback(Some(&maps), 0, phone, false).is_none());
        let desktop = wgpu::Features::TEXTURE_COMPRESSION_BC;
        assert!(usable(Some(&maps), 0, desktop).is_none());
        let decoded = fallback(Some(&maps), 0, desktop, false).expect("astc decodes on the cpu");
        assert_eq!((decoded[0].width, decoded[0].height), (8, 8));
        assert!(
            decoded[0]
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [90, 140, 60, 255]),
            "{:?}",
            &decoded[0].rgba[..8]
        );
        let wrong = crate::texture_budget::wrong_platform_textures();
        assert!(wrong.decoded >= 1, "{wrong:?}");
    }

    /// A void-extent (constant colour) ASTC block of an RGBA8 colour.
    fn void_extent_astc(rgba: [u8; 4]) -> Vec<u8> {
        let mut bits: u128 = 0x1fc | (0b11 << 10) | (((1u128 << 52) - 1) << 12);
        for (c, v) in rgba.iter().enumerate() {
            bits |= u128::from(u16::from(*v) * 257) << (64 + 16 * c);
        }
        bits.to_le_bytes().to_vec()
    }

    /// The decoded fallback of an ASTC normal-mode map (`X X X Y`) binds as an XYZ normal map:
    /// Y moves from A to G and Z is rebuilt.
    #[test]
    fn a_decoded_astc_normal_map_is_xyz() {
        let astc = wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::Unorm,
        };
        let mut t = solid(astc, vec![void_extent_astc([128, 128, 128, 128]).repeat(4)]);
        t.normal_layout = NormalLayout::XInRgbYInA;
        let decoded = t.decode_rgba8().expect("decodes");
        assert_eq!(&decoded.rgba[..4], &[128, 128, 255, 255]);
    }

    /// Each family's path on a desktop GPU (BC), a phone (ASTC + ETC2) and an adapter with
    /// neither: native where the feature is present, else a CPU decode; HDR ASTC has no decoder.
    #[test]
    fn the_texture_path_follows_the_adapter_features() {
        use wgpu::{AstcBlock, AstcChannel, Features as F, TextureFormat as W};
        let astc = |channel| W::Astc {
            block: AstcBlock::B6x6,
            channel,
        };
        let desktop = F::TEXTURE_COMPRESSION_BC;
        let phone = F::TEXTURE_COMPRESSION_ASTC | F::TEXTURE_COMPRESSION_ETC2;
        let old_phone = F::TEXTURE_COMPRESSION_ETC2;
        for (format, features, want) in [
            (W::Bc7RgbaUnormSrgb, desktop, TexturePath::Native),
            (W::Bc7RgbaUnormSrgb, phone, TexturePath::CpuDecode),
            (astc(AstcChannel::UnormSrgb), phone, TexturePath::Native),
            (astc(AstcChannel::UnormSrgb), old_phone, TexturePath::CpuDecode),
            (astc(AstcChannel::Unorm), desktop, TexturePath::CpuDecode),
            (astc(AstcChannel::Hdr), desktop, TexturePath::Unbound),
            (W::Etc2Rgb8UnormSrgb, old_phone, TexturePath::Native),
            (W::Etc2Rgba8Unorm, desktop, TexturePath::CpuDecode),
            (W::EacRg11Unorm, F::empty(), TexturePath::CpuDecode),
            (W::EacRg11Snorm, F::empty(), TexturePath::Unbound),
            (W::Bc6hRgbUfloat, phone, TexturePath::Unbound),
            (W::Bc1RgbaUnorm, F::empty(), TexturePath::CpuDecode),
            (W::Bc4RUnorm, F::empty(), TexturePath::CpuDecode),
            (W::Bc2RgbaUnorm, F::empty(), TexturePath::Unbound),
            (W::Bc4RSnorm, F::empty(), TexturePath::Unbound),
            (W::Bc5RgSnorm, F::empty(), TexturePath::Unbound),
        ] {
            assert_eq!(texture_path(format, features), want, "{format:?} on {features:?}");
        }
        assert_eq!(TexturePath::CpuDecode.label(), "cpu_decode");
        let paths = family_paths(desktop);
        assert_eq!(
            paths.map(|(name, path)| (name, path.label())),
            [("bc", "native"), ("astc", "cpu_decode"), ("etc2", "cpu_decode")]
        );
        assert!(
            family_paths(phone)[1..]
                .iter()
                .all(|(_, p)| *p == TexturePath::Native)
        );
    }

    /// The raw level decode hosts use for their own uploads: the RGBA8 format keeps the sRGB-ness,
    /// no channel is rewritten (an EAC RG11 level keeps B at 0, no rebuilt Z), and a format with
    /// no decoder or a short level is an error.
    #[test]
    fn the_raw_block_decode_keeps_the_sampled_values() {
        use wgpu::TextureFormat as W;
        for (format, want) in [
            (W::Bc7RgbaUnormSrgb, Some(W::Rgba8UnormSrgb)),
            (W::Bc5RgUnorm, Some(W::Rgba8Unorm)),
            (W::Etc2Rgb8UnormSrgb, Some(W::Rgba8UnormSrgb)),
            (W::EacR11Unorm, Some(W::Rgba8Unorm)),
            (
                W::Astc {
                    block: wgpu::AstcBlock::B8x8,
                    channel: wgpu::AstcChannel::UnormSrgb,
                },
                Some(W::Rgba8UnormSrgb),
            ),
            (
                W::Astc {
                    block: wgpu::AstcBlock::B8x8,
                    channel: wgpu::AstcChannel::Hdr,
                },
                None,
            ),
            (W::Bc6hRgbUfloat, None),
        ] {
            assert_eq!(decoded_rgba8_format(format), want, "{format:?}");
        }
        let half = (128u64 << 56) | (13 << 48) | 0o4444_4444_4444_4444;
        let mut block = half.to_be_bytes().to_vec();
        block.extend_from_slice(&half.to_be_bytes());
        let decoded = decode_blocks_rgba8(W::EacRg11Unorm, 4, 4, &block).expect("rg11 decodes");
        assert_eq!(&decoded[..4], &[128, 128, 0, 255]);
        assert!(decode_blocks_rgba8(W::EacRg11Unorm, 4, 4, &block[..8]).is_err());
        assert!(decode_blocks_rgba8(W::Bc6hRgbUfloat, 4, 4, &block).is_err());
    }

    /// Every ETC2 / EAC format the loader maps decodes, and an EAC RG11 normal gets its Z back.
    #[test]
    fn etc2_and_eac_maps_decode() {
        use wgpu::TextureFormat as W;
        for (format, block_bytes) in [
            (W::Etc2Rgb8UnormSrgb, 8),
            (W::Etc2Rgb8A1Unorm, 8),
            (W::Etc2Rgba8UnormSrgb, 16),
            (W::EacR11Unorm, 8),
            (W::EacRg11Unorm, 16),
        ] {
            let t = solid(format, vec![vec![0; 4 * block_bytes]]);
            let decoded = t.decode_rgba8().unwrap_or_else(|| panic!("{format:?} decodes"));
            assert_eq!(decoded.rgba.len(), 8 * 8 * 4, "{format:?}");
        }
        // EAC RG11, both channels: base 128 · 8 + 4 = 1028 of 2047 (multiplier 0, modifier
        // table 13 index 4 is 0): about 128, a flat normal.
        let half = (128u64 << 56) | (13 << 48) | 0o4444_4444_4444_4444;
        let mut block = half.to_be_bytes().to_vec();
        block.extend_from_slice(&half.to_be_bytes());
        let t = CompressedTexture {
            format: W::EacRg11Unorm,
            width: 4,
            height: 4,
            levels: vec![block.into()],
            normal_layout: NormalLayout::Xy,
            mip_stream: None,
        };
        let decoded = t.decode_rgba8().expect("rg11 decodes");
        assert_eq!(&decoded.rgba[..4], &[128, 128, 255, 255]);
    }

    #[test]
    fn the_fallback_decodes_every_cooked_level() {
        let t = solid(
            wgpu::TextureFormat::Bc1RgbaUnorm,
            vec![vec![0; 32], vec![0; 8], vec![0; 8], vec![0; 8]],
        );
        let chain = t.decode_chain_rgba8().expect("chain");
        let sizes: Vec<_> = chain.iter().map(|l| (l.width, l.height, l.rgba.len())).collect();
        assert_eq!(sizes, [(8, 8, 256), (4, 4, 64), (2, 2, 16), (1, 1, 4)]);
        let truncated = solid(wgpu::TextureFormat::Bc1RgbaUnorm, vec![vec![0; 32], vec![0; 4]]);
        assert!(truncated.decode_chain_rgba8().is_none());
    }
}
