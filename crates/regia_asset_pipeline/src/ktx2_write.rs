// SPDX-License-Identifier: MPL-2.0
//! KTX2 writer for BCn, ASTC and ETC2/EAC payloads (Khronos KTX 2.0 specification).

use ktx2::dfd::Block;
use ktx2::{Format, Header, Index, LevelIndex};
use thiserror::Error;

/// Block-compressed format tag for desktop cooking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BcnFormat {
    /// BC1 sRGB (opaque colour, low quality).
    Bc1Srgb,
    /// BC1 linear.
    Bc1Linear,
    /// BC3 sRGB (colour + alpha, low quality).
    Bc3Srgb,
    /// BC3 linear.
    Bc3Linear,
    /// BC7 linear (ORM and other linear colour data; masks use `Bc4`).
    Bc7Linear,
    /// BC7 sRGB (colour textures).
    Bc7Srgb,
    /// BC5 normal maps (RG).
    Bc5,
    /// BC4 single channel linear.
    Bc4,
}

/// ASTC block size for Android cooking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AstcBlock {
    /// ASTC 4×4 (high quality).
    Block4x4,
    /// ASTC 5×5. The texture cook does not select this block (`astc_block_for` uses 4×4 for
    /// normal maps, and 6×6 or 8×8 for low-quality colour and masks).
    Block5x5,
    /// ASTC 6×6 (low-quality colour and linear data, when both edges divide by 6).
    Block6x6,
    /// ASTC 8×8 (low quality / single channel).
    Block8x8,
}

/// Errors building a KTX2 blob.
#[derive(Debug, Error)]
pub enum Ktx2WriteError {
    /// A mip level has unexpected byte length.
    #[error("mip {level} size {actual} != expected {expected}")]
    MipSize {
        /// Mip index.
        level: u32,
        /// Actual bytes.
        actual: usize,
        /// Expected bytes.
        expected: usize,
    },

    /// Width or height is zero.
    #[error("invalid texture dimensions {width}x{height}")]
    InvalidDimensions {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },

    /// DFD generation failed.
    #[error("ktx2 dfd error: {0}")]
    Dfd(String),

    /// Unknown format mapping.
    #[error("unsupported ktx2 format")]
    UnsupportedFormat,
}

impl BcnFormat {
    fn vk_format(self) -> Result<Format, Ktx2WriteError> {
        let v = match self {
            BcnFormat::Bc1Srgb => Format::BC1_RGB_SRGB_BLOCK,
            BcnFormat::Bc1Linear => Format::BC1_RGB_UNORM_BLOCK,
            BcnFormat::Bc3Srgb => Format::BC3_SRGB_BLOCK,
            BcnFormat::Bc3Linear => Format::BC3_UNORM_BLOCK,
            BcnFormat::Bc7Linear => Format::BC7_UNORM_BLOCK,
            BcnFormat::Bc7Srgb => Format::BC7_SRGB_BLOCK,
            BcnFormat::Bc5 => Format::BC5_UNORM_BLOCK,
            BcnFormat::Bc4 => Format::BC4_UNORM_BLOCK,
        };
        Ok(v)
    }

    /// The [`Ktx2Layout`] of this format for a cube map or array.
    ///
    /// # Errors
    ///
    /// `Ktx2WriteError::UnsupportedFormat` for a format with no Vulkan mapping.
    pub fn layout(self, layers: u32, faces: u32) -> Result<Ktx2Layout, Ktx2WriteError> {
        Ok(Ktx2Layout {
            format: self.vk_format()?,
            block: self.block_dims(),
            layers,
            faces,
        })
    }

    fn block_dims(self) -> (u32, u32, usize) {
        match self {
            BcnFormat::Bc4 => (4, 4, 8),
            BcnFormat::Bc1Srgb | BcnFormat::Bc1Linear => (4, 4, 8),
            BcnFormat::Bc3Srgb
            | BcnFormat::Bc3Linear
            | BcnFormat::Bc5
            | BcnFormat::Bc7Linear
            | BcnFormat::Bc7Srgb => (4, 4, 16),
        }
    }
}

impl AstcBlock {
    fn vk_format_srgb(self) -> Format {
        match self {
            AstcBlock::Block4x4 => Format::ASTC_4x4_SRGB_BLOCK,
            AstcBlock::Block5x5 => Format::ASTC_5x5_SRGB_BLOCK,
            AstcBlock::Block6x6 => Format::ASTC_6x6_SRGB_BLOCK,
            AstcBlock::Block8x8 => Format::ASTC_8x8_SRGB_BLOCK,
        }
    }

    fn vk_format_linear(self) -> Format {
        match self {
            AstcBlock::Block4x4 => Format::ASTC_4x4_UNORM_BLOCK,
            AstcBlock::Block5x5 => Format::ASTC_5x5_UNORM_BLOCK,
            AstcBlock::Block6x6 => Format::ASTC_6x6_UNORM_BLOCK,
            AstcBlock::Block8x8 => Format::ASTC_8x8_UNORM_BLOCK,
        }
    }

    pub(crate) fn block_size(self) -> (u32, u32) {
        match self {
            AstcBlock::Block4x4 => (4, 4),
            AstcBlock::Block5x5 => (5, 5),
            AstcBlock::Block6x6 => (6, 6),
            AstcBlock::Block8x8 => (8, 8),
        }
    }
}

/// Pack BCn mip chains into an uncompressed KTX2 file (supercompression: none).
pub fn write_ktx2_bcn(
    width: u32,
    height: u32,
    format: BcnFormat,
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    let vk = format.vk_format()?;
    let (bw, bh, bpb) = format.block_dims();
    write_ktx2_compressed(width, height, vk, (bw, bh, bpb), (0, 1, 1), mips)
}

/// The texel layout of a KTX2 payload written by [`write_ktx2_layered`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ktx2Layout {
    /// The Vulkan format the file declares.
    pub format: Format,
    /// Block width, height (texels) and bytes a block (`1, 1, texel bytes` for an uncompressed format).
    pub block: (u32, u32, usize),
    /// KTX2 `layerCount`: 0 for a plain texture or one cube, the array size (or cube count) otherwise.
    pub layers: u32,
    /// 1 for a 2D texture or array, 6 for a cube.
    pub faces: u32,
}

/// Pack a cube map, a texture array or any other format into KTX2 (supercompression: none).
///
/// `mips[l]` holds every layer and face of level `l` back to back, layer-major then face in the
/// order +X, -X, +Y, -Y, +Z, -Z, each `block`-sized rows of the level. Level data and the data
/// start are 16-byte aligned.
///
/// # Errors
///
/// `Ktx2WriteError`: zero size, a level of the wrong length, a format the DFD writer refuses.
pub fn write_ktx2_layered(
    width: u32,
    height: u32,
    layout: Ktx2Layout,
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    write_ktx2_compressed_aligned(
        width,
        height,
        layout.format,
        layout.block,
        (layout.layers, layout.faces, 16),
        mips,
    )
}

/// Pack ASTC mip chains into KTX2 (`srgb` selects UNORM vs SRGB VkFormat).
pub fn write_ktx2_astc(
    width: u32,
    height: u32,
    block: AstcBlock,
    srgb: bool,
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    let vk = if srgb {
        block.vk_format_srgb()
    } else {
        block.vk_format_linear()
    };
    let (bw, bh) = block.block_size();
    write_ktx2_compressed(width, height, vk, (bw, bh, 16), (0, 1, 1), mips)
}

/// Pack ETC2 / EAC mip chains into KTX2 (`srgb` selects the SRGB VkFormat of the ETC2 colour
/// formats; EAC R11 and RG11 have UNORM only and ignore it).
pub fn write_ktx2_etc2(
    width: u32,
    height: u32,
    kind: crate::etc2_compress::Etc2Kind,
    srgb: bool,
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    use crate::etc2_compress::Etc2Kind;
    let vk = match (kind, srgb) {
        (Etc2Kind::Rgb8, false) => Format::ETC2_R8G8B8_UNORM_BLOCK,
        (Etc2Kind::Rgb8, true) => Format::ETC2_R8G8B8_SRGB_BLOCK,
        (Etc2Kind::Rgba8, false) => Format::ETC2_R8G8B8A8_UNORM_BLOCK,
        (Etc2Kind::Rgba8, true) => Format::ETC2_R8G8B8A8_SRGB_BLOCK,
        (Etc2Kind::R11, _) => Format::EAC_R11_UNORM_BLOCK,
        (Etc2Kind::Rg11, _) => Format::EAC_R11G11_UNORM_BLOCK,
    };
    write_ktx2_compressed(width, height, vk, (4, 4, kind.block_bytes()), (0, 1, 1), mips)
}

fn write_ktx2_compressed(
    width: u32,
    height: u32,
    vk_format: Format,
    block: (u32, u32, usize),
    shape: (u32, u32, usize),
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    write_ktx2_compressed_aligned(width, height, vk_format, block, shape, mips)
}

/// `shape` is `(layer_count, face_count, alignment)`; an alignment of 1 packs the levels tight.
fn write_ktx2_compressed_aligned(
    width: u32,
    height: u32,
    vk_format: Format,
    block: (u32, u32, usize),
    shape: (u32, u32, usize),
    mips: &[Vec<u8>],
) -> Result<Vec<u8>, Ktx2WriteError> {
    let (block_w, block_h, bytes_per_block) = block;
    let (layer_count, face_count, align) = shape;
    let subresources = layer_count.max(1) as usize * face_count as usize;
    if width == 0 || height == 0 {
        return Err(Ktx2WriteError::InvalidDimensions { width, height });
    }
    let level_count = mips.len().max(1) as u32;
    let mut level_bytes: Vec<Vec<u8>> = mips.to_vec();
    if level_bytes.is_empty() {
        level_bytes.push(Vec::new());
    }

    let mut level_index_entries = Vec::with_capacity(level_count as usize);
    let mut payload = Vec::new();
    let mut mip_w = width;
    let mut mip_h = height;
    for (i, data) in level_bytes.iter().enumerate() {
        let blocks_x = mip_w.div_ceil(block_w);
        let blocks_y = mip_h.div_ceil(block_h);
        let expected = (blocks_x * blocks_y) as usize * bytes_per_block * subresources;
        if !data.is_empty() && data.len() != expected {
            return Err(Ktx2WriteError::MipSize {
                level: i as u32,
                actual: data.len(),
                expected,
            });
        }
        // Levels start on `align`; the data start below is aligned the same way.
        payload.resize(payload.len().next_multiple_of(align), 0);
        let offset = payload.len() as u64;
        payload.extend_from_slice(data);
        level_index_entries.push((offset, data.len() as u64));
        mip_w = (mip_w / 2).max(1);
        mip_h = (mip_h / 2).max(1);
    }

    let (basic, type_size) =
        ktx2::dfd::Basic::from_format(vk_format).map_err(|e| Ktx2WriteError::Dfd(e.to_string()))?;
    let dfd_block = Block::Basic(basic);
    let dfd_body = dfd_block.to_vec();
    let dfd_total_len = 4u32 + dfd_body.len() as u32;

    let level_index_len = (level_count as usize) * LevelIndex::LENGTH;
    let level_index_offset = Header::LENGTH;
    let dfd_offset = level_index_offset + level_index_len;
    let kvd_offset = dfd_offset + dfd_total_len as usize;
    let data_offset = kvd_offset.next_multiple_of(align);

    let mut level_indices = Vec::with_capacity(level_count as usize);
    for (rel_offset, len) in level_index_entries {
        level_indices.push(LevelIndex {
            byte_offset: data_offset as u64 + rel_offset,
            byte_length: len,
            uncompressed_byte_length: len,
        });
    }

    let header = Header {
        format: Some(vk_format),
        type_size,
        pixel_width: width,
        pixel_height: height,
        pixel_depth: 0,
        layer_count,
        face_count,
        level_count,
        supercompression_scheme: None,
        index: Index {
            dfd_byte_offset: dfd_offset as u32,
            dfd_byte_length: dfd_total_len,
            kvd_byte_offset: kvd_offset as u32,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };

    let mut out = Vec::with_capacity(data_offset + payload.len());
    out.extend_from_slice(&header.as_bytes());
    for level in &level_indices {
        out.extend_from_slice(&level.as_bytes());
    }
    out.extend_from_slice(&dfd_total_len.to_le_bytes());
    out.extend_from_slice(&dfd_body);
    out.resize(data_offset, 0);
    out.extend_from_slice(&payload);

    // Validate with the same reader the runtime uses.
    ktx2::Reader::new(&out).map_err(|e| Ktx2WriteError::Dfd(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etc2_and_eac_ktx2_pass_the_reader_with_their_formats() {
        use crate::etc2_compress::Etc2Kind;
        for (kind, srgb, format) in [
            (Etc2Kind::Rgb8, true, Format::ETC2_R8G8B8_SRGB_BLOCK),
            (Etc2Kind::Rgb8, false, Format::ETC2_R8G8B8_UNORM_BLOCK),
            (Etc2Kind::Rgba8, true, Format::ETC2_R8G8B8A8_SRGB_BLOCK),
            (Etc2Kind::R11, true, Format::EAC_R11_UNORM_BLOCK),
            (Etc2Kind::Rg11, false, Format::EAC_R11G11_UNORM_BLOCK),
        ] {
            // 8×8 with three levels (8, 4, 2): 4 blocks, 1, 1.
            let b = kind.block_bytes();
            let mips = vec![vec![0u8; 4 * b], vec![0u8; b], vec![0u8; b]];
            let blob = write_ktx2_etc2(8, 8, kind, srgb, &mips).expect("write");
            let reader = ktx2::Reader::new(&blob).expect("reader");
            assert_eq!(reader.header().format, Some(format));
            assert_eq!(reader.levels().count(), 3);
            // A level of the wrong size is refused.
            assert!(write_ktx2_etc2(8, 8, kind, srgb, &[vec![0u8; b]]).is_err());
        }
    }

    #[test]
    fn bcn7_ktx2_passes_ktx2_reader() {
        let w = 4u32;
        let h = 4u32;
        let block_bytes = 16usize;
        let mips = vec![vec![0u8; block_bytes]];
        let blob = write_ktx2_bcn(w, h, BcnFormat::Bc7Srgb, &mips).expect("write");
        let reader = ktx2::Reader::new(&blob).expect("reader");
        assert_eq!(reader.header().pixel_width, w);
        assert_eq!(reader.levels().count(), 1);
    }
}
