// SPDX-License-Identifier: MPL-2.0
//! The DDS texture importer (`docs/specs/95-loudness-importers.md` § 3, ADR-1855).
//!
//! Reads the container layout Microsoft documents publicly ("DDS file layout", `DDS_HEADER`,
//! `DDS_PIXELFORMAT`, `DDS_HEADER_DXT10`): 2D textures, cube maps (the DX9 caps2 flags or the DX10
//! cube misc flag, also cube arrays) and texture arrays (DX10 `arraySize`) in BC1, BC2, BC3, BC4
//! (unsigned and signed), BC5 (unsigned and signed), BC6H (UF16 and SF16), BC7 and 32-bit
//! uncompressed RGBA/BGRA, with any mip chain.
//!
//! - A 2D BC1/BC3/BC4/BC5/BC7 file with two or more levels keeps its blocks when `compress` is not
//!   `Some(false)` (desktop: copied into the cooked KTX2; Android: transcoded to ASTC).
//!   `compress: Some(false)` decodes level 0 to RGBA8. `max_size` drops leading block levels and
//!   always keeps the last, so a short chain can cook an edge larger than the cap. Other 2D LDR
//!   files are decoded to RGBA8 and cooked like a PNG.
//! - A cube map or array is cooked to one KTX2 (`faceCount` 6 and/or `layerCount`), the file's
//!   mip chain kept: BC blocks pass through on desktop and are transcoded to ASTC per face on
//!   Android; formats without a pass-through (BC2, RGBA8, BC1 with punch-through alpha, signed
//!   BC4/BC5) are decoded and stored as uncompressed RGBA8.
//! - BC6H passes through as `BC6H_UFLOAT`/`BC6H_SFLOAT` where the target takes BC; on Android (or
//!   with `hdr_encoding` set to `rgba16f` or `astc_hdr`) it is decoded to float and re-encoded
//!   like an HDR image source (RGBA16F by default there, with the size warning).
//! - Signed BC4/BC5 are decoded and remapped to unsigned (`v * 0.5 + 0.5`) before the usual
//!   normal-map or mask cook, so the cooked texture is the engine's UNORM BC5/BC4 (the renderer's
//!   material loader has no SNORM format).
//! - Volume textures stay refused (`dds_unsupported`): there is no cooked 3D form.

use std::path::Path;

use regia_asset::{AssetId, CookedImage, CookedTexture, TextureCookUsage};

use crate::cook::CookError;
use crate::db::AssetDatabase;
use crate::ktx2_write::{BcnFormat, Ktx2Layout, write_ktx2_bcn, write_ktx2_layered};
use crate::texture_compress::TextureCookQuality;
use crate::texture_settings::{TextureImporterSettings, usage_from_file_name};

/// The importer name the cook records for DDS sources.
pub const DDS_IMPORTER: &str = "dds";

const MAGIC: &[u8; 4] = b"DDS ";
const HEADER_LEN: usize = 124;
const DX10_LEN: usize = 20;
const DDSD_MIPMAPCOUNT: u32 = 0x2_0000;
const DDPF_ALPHAPIXELS: u32 = 0x1;
const DDPF_FOURCC: u32 = 0x4;
const DDPF_RGB: u32 = 0x40;
const DDSCAPS2_CUBEMAP: u32 = 0x200;
/// The six `DDSCAPS2_CUBEMAP_{POSITIVE,NEGATIVE}{X,Y,Z}` face flags.
const DDSCAPS2_CUBEMAP_ALL_FACES: u32 = 0xFC00;
const DDSCAPS2_VOLUME: u32 = 0x20_0000;
const D3D10_RESOURCE_DIMENSION_TEXTURE2D: u32 = 3;
const D3D10_RESOURCE_DIMENSION_TEXTURE3D: u32 = 4;
/// The largest array size (or cube count) accepted (the D3D11 limit).
const MAX_ARRAY: u32 = 2048;
const D3D10_RESOURCE_MISC_TEXTURECUBE: u32 = 0x4;
/// The largest edge accepted, texels (the cook's own texture limit is far below).
const MAX_EDGE: u32 = 1 << 15;

/// The pixel layout of a DDS image's levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DdsFormat {
    /// BC1 (DXT1), 8 bytes a 4×4 block.
    Bc1,
    /// BC2 (DXT3), 16 bytes: explicit 4-bit alpha and a BC1 color block.
    Bc2,
    /// BC3 (DXT5), 16 bytes.
    Bc3,
    /// BC4 unsigned, 8 bytes, one channel.
    Bc4,
    /// BC5 unsigned, 16 bytes, two channels.
    Bc5,
    /// BC4 signed (`BC4_SNORM`), 8 bytes.
    Bc4Snorm,
    /// BC5 signed (`BC5_SNORM`), 16 bytes: the usual tangent-space normal map from a DCC tool.
    Bc5Snorm,
    /// BC6H unsigned half float (`BC6H_UF16`), 16 bytes.
    Bc6hUf16,
    /// BC6H signed half float (`BC6H_SF16`), 16 bytes.
    Bc6hSf16,
    /// BC7, 16 bytes.
    Bc7,
    /// Uncompressed RGBA8 (BGRA and legacy masked layouts are swizzled to it on parse).
    Rgba8,
}

impl DdsFormat {
    /// Bytes of one block (BC) or one texel (RGBA8), and whether the format is block-compressed.
    const fn unit(self) -> (usize, bool) {
        match self {
            Self::Bc1 | Self::Bc4 | Self::Bc4Snorm => (8, true),
            Self::Bc2
            | Self::Bc3
            | Self::Bc5
            | Self::Bc5Snorm
            | Self::Bc6hUf16
            | Self::Bc6hSf16
            | Self::Bc7 => (16, true),
            Self::Rgba8 => (4, false),
        }
    }

    /// Bytes of a `width × height` level.
    #[must_use]
    pub fn level_len(self, width: u32, height: u32) -> usize {
        let (unit, blocks) = self.unit();
        let (w, h) = (width.max(1) as usize, height.max(1) as usize);
        if blocks {
            w.div_ceil(4) * h.div_ceil(4) * unit
        } else {
            w * h * unit
        }
    }
}

/// A parsed DDS 2D texture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DdsImage {
    /// Width of level 0, texels.
    pub width: u32,
    /// Height of level 0, texels.
    pub height: u32,
    /// Layout of every level.
    pub format: DdsFormat,
    /// Color space the file states: `Some(true)` for a DX10 `_SRGB` format, `Some(false)` for a
    /// DX10 `_UNORM` one, `None` when the file does not say (legacy FourCC, typeless).
    pub srgb: Option<bool>,
    /// KTX2 `layerCount`: 0 for a plain texture or a single cube map, the array size (or the
    /// number of cubes) otherwise. A DX10 `arraySize` of 1 is a plain texture.
    pub layers: u32,
    /// 1 for a 2D texture or array, 6 for a cube map.
    pub faces: u32,
    /// The color is premultiplied by alpha: the legacy DXT2 (BC2) and DXT4 (BC3) FourCCs, which
    /// store premultiplied color, unlike DXT3/DXT5. Premultiplied files are decoded and
    /// unpremultiplied, never passed through.
    pub premultiplied: bool,
    /// The levels, largest first; level `i` is `max(1, width >> i) × max(1, height >> i)`. Each
    /// entry holds every layer and face of that level back to back, layer-major then face (+X,
    /// -X, +Y, -Y, +Z, -Z), which is the KTX2 level layout.
    pub levels: Vec<Vec<u8>>,
}

/// Why a DDS file was refused. The display text starts with the error's code.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DdsError {
    /// Not a DDS file, or its header is inconsistent.
    #[error("dds_bad_header: {0}")]
    BadHeader(String),
    /// A valid file of a kind the importer does not read.
    #[error("dds_unsupported: {0}")]
    Unsupported(String),
    /// The file ends before its last level.
    #[error("dds_truncated: level {level} needs {needed} bytes, {available} remain")]
    Truncated {
        /// The level that does not fit.
        level: usize,
        /// Its byte length.
        needed: usize,
        /// The bytes left in the file.
        available: usize,
    },
}

impl DdsError {
    /// The machine-readable code (`dds_bad_header`, `dds_unsupported`, `dds_truncated`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadHeader(_) => "dds_bad_header",
            Self::Unsupported(_) => "dds_unsupported",
            Self::Truncated { .. } => "dds_truncated",
        }
    }
}

/// The refusal for a volume (3D) texture: the cooked texture has no 3D form.
fn volume_refusal() -> DdsError {
    DdsError::Unsupported(
        "volume (3D) textures have no cooked form; export the slices as a texture array or an atlas".into(),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// The format of a DX10 header's `DXGI_FORMAT`, its color space, and whether the source is BGRA.
fn dxgi_format(dxgi: u32) -> Result<(DdsFormat, Option<bool>, bool), DdsError> {
    use DdsFormat::*;
    Ok(match dxgi {
        27 => (Rgba8, None, false),
        28 => (Rgba8, Some(false), false),
        29 => (Rgba8, Some(true), false),
        87 => (Rgba8, Some(false), true),
        90 => (Rgba8, None, true),
        91 => (Rgba8, Some(true), true),
        70 => (Bc1, None, false),
        71 => (Bc1, Some(false), false),
        72 => (Bc1, Some(true), false),
        73 => (Bc2, None, false),
        74 => (Bc2, Some(false), false),
        75 => (Bc2, Some(true), false),
        76 => (Bc3, None, false),
        77 => (Bc3, Some(false), false),
        78 => (Bc3, Some(true), false),
        79 | 80 => (Bc4, Some(false), false),
        81 => (Bc4Snorm, Some(false), false),
        82 | 83 => (Bc5, Some(false), false),
        84 => (Bc5Snorm, Some(false), false),
        94..=95 => (Bc6hUf16, Some(false), false),
        96 => (Bc6hSf16, Some(false), false),
        97 => (Bc7, None, false),
        98 => (Bc7, Some(false), false),
        99 => (Bc7, Some(true), false),
        other => return Err(DdsError::Unsupported(format!("DXGI format {other}"))),
    })
}

/// How a legacy 32-bit masked layout maps to RGBA: the shift of each channel (`None` = alpha
/// absent, read as opaque).
struct Masks {
    shifts: [Option<u32>; 4],
}

fn masks(pf: &[u8]) -> Result<Masks, DdsError> {
    let flags = u32_at(pf, 4);
    let bits = u32_at(pf, 12);
    if bits != 32 {
        return Err(DdsError::Unsupported(format!("{bits}-bit uncompressed layout")));
    }
    let mut shifts = [None; 4];
    for (i, shift) in shifts.iter_mut().enumerate() {
        let mask = u32_at(pf, 16 + 4 * i);
        if i == 3 && (mask == 0 || flags & DDPF_ALPHAPIXELS == 0) {
            continue;
        }
        let shift_by = mask.trailing_zeros();
        if mask == 0 || mask >> shift_by != 0xFF {
            return Err(DdsError::Unsupported(format!("channel mask {mask:#010x}")));
        }
        *shift = Some(shift_by);
    }
    Ok(Masks { shifts })
}

fn swizzle(level: &[u8], masks: &Masks) -> Vec<u8> {
    let mut out = Vec::with_capacity(level.len());
    for px in level.as_chunks::<4>().0 {
        let v = u32::from_le_bytes(*px);
        for shift in masks.shifts {
            out.push(shift.map_or(0xFF, |s| ((v >> s) & 0xFF) as u8));
        }
    }
    out
}

/// Parses a DDS file.
///
/// # Errors
///
/// [`DdsError`]: not a DDS file, a volume texture, a cube map missing a face, an unsupported
/// format, or a file shorter than its mip chain.
pub fn parse_dds(bytes: &[u8]) -> Result<DdsImage, DdsError> {
    if bytes.len() < 4 + HEADER_LEN || &bytes[..4] != MAGIC {
        return Err(DdsError::BadHeader(
            "not a DDS file (no `DDS ` magic and 124-byte header)".into(),
        ));
    }
    let h = &bytes[4..4 + HEADER_LEN];
    if u32_at(h, 0) != HEADER_LEN as u32 || u32_at(h, 72) != 32 {
        return Err(DdsError::BadHeader(
            "header or pixel-format size field is wrong".into(),
        ));
    }
    let flags = u32_at(h, 4);
    let (height, width) = (u32_at(h, 8), u32_at(h, 12));
    if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
        return Err(DdsError::BadHeader(format!(
            "size {width}×{height} is outside 1..={MAX_EDGE}"
        )));
    }
    let caps2 = u32_at(h, 108);
    if caps2 & DDSCAPS2_VOLUME != 0 {
        return Err(volume_refusal());
    }
    let mut faces = 1;
    let mut layers = 0;
    if caps2 & DDSCAPS2_CUBEMAP != 0 {
        if caps2 & DDSCAPS2_CUBEMAP_ALL_FACES != DDSCAPS2_CUBEMAP_ALL_FACES {
            return Err(DdsError::Unsupported(
                "cube map without all six faces (the cooked cube needs +X, -X, +Y, -Y, +Z and -Z)".into(),
            ));
        }
        if width != height {
            return Err(DdsError::BadHeader(format!(
                "cube faces are {width}×{height}, not square"
            )));
        }
        faces = 6;
    }
    let full_chain = 32 - width.max(height).leading_zeros();
    let mips = if flags & DDSD_MIPMAPCOUNT != 0 {
        u32_at(h, 24).max(1)
    } else {
        1
    };
    if mips > full_chain {
        return Err(DdsError::BadHeader(format!(
            "{mips} mip levels for a {width}×{height} texture (at most {full_chain})"
        )));
    }
    let pf = &h[72..104];
    let pf_flags = u32_at(pf, 4);
    let mut data_start = 4 + HEADER_LEN;
    let mut legacy_masks = None;
    let mut bgra = false;
    let (format, srgb, premultiplied) = if pf_flags & DDPF_FOURCC != 0 {
        match &pf[8..12] {
            b"DXT1" => (DdsFormat::Bc1, None, false),
            // DXT2 and DXT4 store color premultiplied by alpha; DXT3 and DXT5 store it straight.
            b"DXT2" => (DdsFormat::Bc2, None, true),
            b"DXT3" => (DdsFormat::Bc2, None, false),
            b"DXT4" => (DdsFormat::Bc3, None, true),
            b"DXT5" => (DdsFormat::Bc3, None, false),
            b"ATI1" | b"BC4U" => (DdsFormat::Bc4, Some(false), false),
            b"ATI2" | b"BC5U" => (DdsFormat::Bc5, Some(false), false),
            b"DX10" => {
                let dx10 = bytes
                    .get(data_start..data_start + DX10_LEN)
                    .ok_or_else(|| DdsError::BadHeader("DX10 header is missing".into()))?;
                data_start += DX10_LEN;
                let (format, srgb, is_bgra) = dxgi_format(u32_at(dx10, 0))?;
                match u32_at(dx10, 4) {
                    D3D10_RESOURCE_DIMENSION_TEXTURE2D => {}
                    D3D10_RESOURCE_DIMENSION_TEXTURE3D => return Err(volume_refusal()),
                    other => {
                        return Err(DdsError::Unsupported(format!(
                            "resource dimension {other} (only 2D textures, cube maps and arrays are imported)"
                        )));
                    }
                }
                let array_size = u32_at(dx10, 12);
                // An array size of 0 reads as one texture, as before cubes and arrays were imported
                // (spec 95 § 3 test `dx10_cube_and_array_fields_are_read_and_other_dimensions_refused`).
                if array_size > MAX_ARRAY {
                    return Err(DdsError::BadHeader(format!(
                        "array size {array_size} is above {MAX_ARRAY}"
                    )));
                }
                if u32_at(dx10, 8) & D3D10_RESOURCE_MISC_TEXTURECUBE != 0 {
                    if width != height {
                        return Err(DdsError::BadHeader(format!(
                            "cube faces are {width}×{height}, not square"
                        )));
                    }
                    faces = 6;
                }
                // One element is a plain texture (or a single cube).
                layers = if array_size > 1 { array_size } else { 0 };
                bgra = is_bgra;
                (format, srgb, false)
            }
            other => {
                return Err(DdsError::Unsupported(format!(
                    "FourCC `{}`",
                    String::from_utf8_lossy(other).escape_default()
                )));
            }
        }
    } else if pf_flags & DDPF_RGB != 0 {
        legacy_masks = Some(masks(pf)?);
        (DdsFormat::Rgba8, None, false)
    } else {
        return Err(DdsError::Unsupported("neither a FourCC nor an RGB layout".into()));
    };
    let bgra_masks = Masks {
        shifts: [Some(16), Some(8), Some(0), Some(24)],
    };
    // The file stores each array element, then each face, then its mip chain; the image keeps
    // one buffer a level (KTX2 order), so the subresources append in file order.
    let mut levels = vec![Vec::new(); mips as usize];
    let mut offset = data_start;
    for _subresource in 0..layers.max(1) * faces {
        for (level, out) in levels.iter_mut().enumerate() {
            let (w, h) = ((width >> level).max(1), (height >> level).max(1));
            let needed = format.level_len(w, h);
            let available = bytes.len().saturating_sub(offset);
            if needed > available {
                return Err(DdsError::Truncated {
                    level,
                    needed,
                    available,
                });
            }
            let raw = &bytes[offset..offset + needed];
            offset += needed;
            match (&legacy_masks, bgra) {
                (Some(m), _) => out.extend_from_slice(&swizzle(raw, m)),
                (None, true) => out.extend_from_slice(&swizzle(raw, &bgra_masks)),
                (None, false) => out.extend_from_slice(raw),
            }
        }
    }
    Ok(DdsImage {
        width,
        height,
        format,
        srgb,
        layers,
        faces,
        premultiplied,
        levels,
    })
}

/// 5:6:5 → 8:8:8 (bit replication).
fn rgb565(c: u16) -> [u8; 3] {
    let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
    [
        ((r << 3) | (r >> 2)) as u8,
        ((g << 2) | (g >> 4)) as u8,
        ((b << 3) | (b >> 2)) as u8,
    ]
}

/// Decodes BC2 blocks to RGBA8: the 4-bit alpha of each texel (low nibble first, expanded ×17)
/// and the color block always in its four-color mode (as BC2 and BC3 define it).
fn decode_bc2(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let bw = w.div_ceil(4);
    let mut out = vec![0u8; w * h * 4];
    for (i, block) in data.as_chunks::<16>().0.iter().enumerate() {
        let (bx, by) = (i % bw, i / bw);
        let alpha = u64::from_le_bytes([
            block[0], block[1], block[2], block[3], block[4], block[5], block[6], block[7],
        ]);
        let c0 = u16::from_le_bytes([block[8], block[9]]);
        let c1 = u16::from_le_bytes([block[10], block[11]]);
        let (a, b) = (rgb565(c0), rgb565(c1));
        let mix = |x: u8, y: u8, wx: u16, wy: u16| ((u16::from(x) * wx + u16::from(y) * wy) / 3) as u8;
        let palette = [
            a,
            b,
            [
                mix(a[0], b[0], 2, 1),
                mix(a[1], b[1], 2, 1),
                mix(a[2], b[2], 2, 1),
            ],
            [
                mix(a[0], b[0], 1, 2),
                mix(a[1], b[1], 1, 2),
                mix(a[2], b[2], 1, 2),
            ],
        ];
        let indices = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);
        for t in 0..16 {
            let (x, y) = (bx * 4 + t % 4, by * 4 + t / 4);
            if x >= w || y >= h {
                continue;
            }
            let rgb = palette[((indices >> (2 * t)) & 3) as usize];
            let alpha4 = ((alpha >> (4 * t)) & 0xF) as u8;
            let o = (y * w + x) * 4;
            out[o..o + 3].copy_from_slice(&rgb);
            out[o + 3] = alpha4 * 17;
        }
    }
    out
}

/// The 8 values of a signed BC4 block's channel, as the public block description defines them:
/// two `i8` endpoints (`-128` reads as `-127`), then six interpolants when `r0 > r1`, otherwise
/// four plus the extremes `-1` and `1`; every value is the integer over 127, clamped to ±1.
fn bc4_snorm_palette(r0: i8, r1: i8) -> [f32; 8] {
    let (a, b) = (f32::from(r0.max(-127)) / 127.0, f32::from(r1.max(-127)) / 127.0);
    let mut p = [a, b, 0.0, 0.0, 0.0, 0.0, -1.0, 1.0];
    if r0 > r1 {
        for i in 1..7 {
            p[1 + i] = ((7 - i) as f32 * a + i as f32 * b) / 7.0;
        }
    } else {
        for i in 1..5 {
            p[1 + i] = ((5 - i) as f32 * a + i as f32 * b) / 5.0;
        }
    }
    p
}

/// Decodes one 8-byte signed BC4 block (or half of a BC5 block) to 16 values in -1..=1, row-major.
fn decode_bc4_snorm_block(block: &[u8]) -> [f32; 16] {
    let mut b = [0u8; 8];
    for (dst, src) in b.iter_mut().zip(block) {
        *dst = *src;
    }
    let palette = bc4_snorm_palette(b[0] as i8, b[1] as i8);
    let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
    let mut out = [0f32; 16];
    for (t, v) in out.iter_mut().enumerate() {
        *v = palette[((bits >> (3 * t)) & 7) as usize];
    }
    out
}

/// -1..=1 → 0..=255 (`v * 0.5 + 0.5`).
fn snorm_to_unorm8(v: f32) -> u8 {
    ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8
}

/// Decodes a signed BC4 (`two = false`, grey) or BC5 (`two = true`, X and Y with Z rebuilt) level
/// to RGBA8, every channel remapped from -1..=1 to 0..=255.
fn decode_snorm(width: u32, height: u32, data: &[u8], two: bool) -> Vec<u8> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let bw = w.div_ceil(4);
    let unit = if two { 16 } else { 8 };
    let mut out = vec![255u8; w * h * 4];
    for (i, block) in data.chunks_exact(unit).enumerate() {
        let (bx, by) = (i % bw, i / bw);
        let r = decode_bc4_snorm_block(&block[..8]);
        let g = if two {
            decode_bc4_snorm_block(&block[8..])
        } else {
            r
        };
        for t in 0..16 {
            let (x, y) = (bx * 4 + t % 4, by * 4 + t / 4);
            if x >= w || y >= h {
                continue;
            }
            let o = (y * w + x) * 4;
            out[o] = snorm_to_unorm8(r[t]);
            if two {
                let z = (1.0 - r[t] * r[t] - g[t] * g[t]).max(0.0).sqrt();
                out[o + 1] = snorm_to_unorm8(g[t]);
                out[o + 2] = snorm_to_unorm8(z);
            } else {
                out[o + 1] = out[o];
                out[o + 2] = out[o];
            }
        }
    }
    out
}

/// Divides RGB by alpha in place: DXT2/DXT4 store color premultiplied by alpha. Texels with
/// alpha 0 keep their RGB.
fn unpremultiply_rgba8(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            // `None` only for alpha 0, whose RGB is kept.
            if let Some(straight) = (u16::from(*c) * 255 + a / 2).checked_div(a) {
                *c = straight.min(255) as u8;
            }
        }
    }
}

impl DdsImage {
    /// How many layer-and-face images each level holds.
    #[must_use]
    pub fn subresources(&self) -> usize {
        self.layers.max(1) as usize * self.faces as usize
    }

    /// True for BC6H, which cooks to a float texture.
    #[must_use]
    pub fn is_hdr(&self) -> bool {
        matches!(self.format, DdsFormat::Bc6hUf16 | DdsFormat::Bc6hSf16)
    }

    /// True for a cube map or a texture array.
    #[must_use]
    pub fn is_layered(&self) -> bool {
        self.layers > 0 || self.faces > 1
    }

    /// The bytes of one layer-and-face image of `level` (`subresource` counts layer-major, then
    /// face), or `None` outside the file.
    #[must_use]
    pub fn subresource(&self, level: usize, subresource: usize) -> Option<&[u8]> {
        let data = self.levels.get(level)?;
        let size = data.len() / self.subresources().max(1);
        data.get(subresource * size..(subresource + 1) * size)
    }

    /// Level `level` of the first image decoded to RGBA8: its width, height and pixels.
    ///
    /// # Errors
    ///
    /// No such level, or the BC decoder refused the level's data.
    pub fn decode_level_rgba8(&self, level: usize) -> Result<(u32, u32, Vec<u8>), String> {
        self.decode_subresource_rgba8(level, 0)
    }

    /// One layer-and-face image of `level` of a BC6H file decoded to `width × height × 3` floats.
    ///
    /// # Errors
    ///
    /// Not a BC6H file, no such level or image, or the data is too short.
    pub fn decode_subresource_rgb(
        &self,
        level: usize,
        subresource: usize,
    ) -> Result<(u32, u32, Vec<f32>), String> {
        use crate::bc6h_encode::{Bc6hFormat, decode_image};
        let format = match self.format {
            DdsFormat::Bc6hUf16 => Bc6hFormat::Unsigned,
            DdsFormat::Bc6hSf16 => Bc6hFormat::Signed,
            _ => return Err("not a BC6H file".into()),
        };
        let data = self
            .subresource(level, subresource)
            .ok_or_else(|| format!("no level {level} image {subresource}"))?;
        let (w, h) = ((self.width >> level).max(1), (self.height >> level).max(1));
        decode_image(w, h, data, format)
            .map(|rgb| (w, h, rgb))
            .map_err(|e| e.to_string())
    }

    /// One layer-and-face image of `level` decoded to RGBA8. Signed BC4/BC5 come out remapped to
    /// unsigned (`v * 0.5 + 0.5`).
    ///
    /// # Errors
    ///
    /// No such level or image, a BC6H file (float; see `decode_subresource_rgb`), or the BC
    /// decoder refused the data.
    pub fn decode_subresource_rgba8(
        &self,
        level: usize,
        subresource: usize,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        use regia_asset::bcn_decode::{BcnKind, decode_bcn};
        let data = self.subresource(level, subresource).ok_or_else(|| {
            format!(
                "no level {level} image {subresource} (the file has {} levels of {} images)",
                self.levels.len(),
                self.subresources()
            )
        })?;
        let (w, h) = ((self.width >> level).max(1), (self.height >> level).max(1));
        let kind = match self.format {
            DdsFormat::Rgba8 => {
                let mut rgba = data.to_vec();
                if self.premultiplied {
                    unpremultiply_rgba8(&mut rgba);
                }
                return Ok((w, h, rgba));
            }
            DdsFormat::Bc2 => {
                let mut rgba = decode_bc2(w, h, data);
                if self.premultiplied {
                    unpremultiply_rgba8(&mut rgba);
                }
                return Ok((w, h, rgba));
            }
            DdsFormat::Bc4Snorm => return Ok((w, h, decode_snorm(w, h, data, false))),
            DdsFormat::Bc5Snorm => return Ok((w, h, decode_snorm(w, h, data, true))),
            DdsFormat::Bc6hUf16 | DdsFormat::Bc6hSf16 => {
                return Err("BC6H is a float format; it has no RGBA8 decode".into());
            }
            DdsFormat::Bc1 => BcnKind::Bc1,
            DdsFormat::Bc3 => BcnKind::Bc3,
            DdsFormat::Bc4 => BcnKind::Bc4,
            DdsFormat::Bc5 => BcnKind::Bc5,
            DdsFormat::Bc7 => BcnKind::Bc7,
        };
        let mut rgba = decode_bcn(kind, w, h, data).map_err(|e| e.to_string())?;
        if self.premultiplied {
            unpremultiply_rgba8(&mut rgba);
        }
        Ok((w, h, rgba))
    }

    /// True when a BC1 level uses punch-through alpha: a block in its three-color mode
    /// (`c0 <= c1`) with a texel at index 3 (transparent). The cooked BC1 formats are opaque, so
    /// such a file is decoded and recooked with alpha instead of passed through.
    #[must_use]
    pub fn has_punch_through_alpha(&self) -> bool {
        self.format == DdsFormat::Bc1
            && self.levels.iter().any(|level| {
                level.as_chunks::<8>().0.iter().any(|b| {
                    let (c0, c1) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
                    let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                    c0 <= c1 && (0..16).any(|t| (idx >> (2 * t)) & 3 == 3)
                })
            })
    }

    /// The cooked BC format the blocks pass through as, for a texture of `usage` when the file
    /// does not state its color space. `None` for BC2, RGBA8, signed BC4/BC5, BC6H (cooked by their
    /// own paths), a premultiplied DXT2/DXT4 file and a BC1 with punch-through alpha (decoded
    /// instead).
    #[must_use]
    pub fn passthrough_format(&self, usage: TextureCookUsage) -> Option<BcnFormat> {
        if self.premultiplied || self.has_punch_through_alpha() {
            return None;
        }
        let srgb = self.srgb.unwrap_or(usage == TextureCookUsage::ColorSrgb);
        Some(match (self.format, srgb) {
            (DdsFormat::Bc1, true) => BcnFormat::Bc1Srgb,
            (DdsFormat::Bc1, false) => BcnFormat::Bc1Linear,
            (DdsFormat::Bc3, true) => BcnFormat::Bc3Srgb,
            (DdsFormat::Bc3, false) => BcnFormat::Bc3Linear,
            (DdsFormat::Bc7, true) => BcnFormat::Bc7Srgb,
            (DdsFormat::Bc7, false) => BcnFormat::Bc7Linear,
            (DdsFormat::Bc4, _) => BcnFormat::Bc4,
            (DdsFormat::Bc5, _) => BcnFormat::Bc5,
            (
                DdsFormat::Bc2
                | DdsFormat::Rgba8
                | DdsFormat::Bc4Snorm
                | DdsFormat::Bc5Snorm
                | DdsFormat::Bc6hUf16
                | DdsFormat::Bc6hSf16,
                _,
            ) => return None,
        })
    }
}

/// The usage a pass-through BC format is sampled as.
fn passthrough_usage(format: BcnFormat) -> TextureCookUsage {
    match format {
        BcnFormat::Bc1Srgb | BcnFormat::Bc3Srgb | BcnFormat::Bc7Srgb => TextureCookUsage::ColorSrgb,
        BcnFormat::Bc1Linear | BcnFormat::Bc3Linear | BcnFormat::Bc7Linear => TextureCookUsage::ColorLinear,
        BcnFormat::Bc5 => TextureCookUsage::NormalMap,
        BcnFormat::Bc4 => TextureCookUsage::Mask,
    }
}

/// The cooked output of one DDS source: settings hash, id, type path, payload.
pub(crate) struct DdsCook {
    pub(crate) settings_hash: [u8; 32],
    pub(crate) id: AssetId,
    pub(crate) type_path: String,
    pub(crate) payload: Vec<u8>,
}

/// The leading levels `max_size` drops (always keeping the last).
fn levels_to_skip(dds: &DdsImage, settings: &TextureImporterSettings) -> usize {
    let max = settings.max_size.filter(|m| *m > 0).unwrap_or(u32::MAX);
    (0..dds.levels.len() - 1)
        .take_while(|&i| (dds.width >> i).max(dds.height >> i) > max)
        .count()
}

fn dds_err(what: impl std::fmt::Display) -> CookError {
    CookError::Dds(format!("dds_unsupported: {what}"))
}

/// A signed half-float's bits (`f32_to_f16_bits` keeps only non-negative values).
fn signed_f16_bits(v: f32) -> u16 {
    use regia_asset::hdr_blocks::f32_to_f16_bits;
    if v.is_sign_negative() && v != 0.0 {
        0x8000 | f32_to_f16_bits(-v)
    } else {
        f32_to_f16_bits(v)
    }
}

/// The KTX2 of a BC6H file (2D, cube or array): blocks passed through where the target takes BC
/// (`hdr_encoding` `bc6h`, the default off Android), otherwise decoded to float and re-encoded as
/// RGBA16F (the default on Android; a signed source keeps its negatives) or ASTC 4×4 HDR (which
/// stores no negatives: they clamp to 0).
fn hdr_container(
    dds: &DdsImage,
    skip: usize,
    settings: &TextureImporterSettings,
    platform: &str,
) -> Result<Vec<u8>, CookError> {
    use crate::hdr_import::HdrEncoding;
    use regia_asset::hdr_blocks::{HdrBlockFormat, encode_hdr_blocks};
    let (w, h) = ((dds.width >> skip).max(1), (dds.height >> skip).max(1));
    let encoding = HdrEncoding::choose(settings, platform);
    let (format, block, bpb) = match encoding {
        HdrEncoding::Bc6h if dds.format == DdsFormat::Bc6hSf16 => {
            (ktx2::Format::BC6H_SFLOAT_BLOCK, (4, 4), 16)
        }
        HdrEncoding::Bc6h => (ktx2::Format::BC6H_UFLOAT_BLOCK, (4, 4), 16),
        HdrEncoding::Rgba16f => (ktx2::Format::R16G16B16A16_SFLOAT, (1, 1), 8),
        HdrEncoding::AstcHdr => (ktx2::Format::ASTC_4x4_SFLOAT_BLOCK, (4, 4), 16),
    };
    let mut mips = Vec::with_capacity(dds.levels.len() - skip);
    for level in skip..dds.levels.len() {
        if encoding == HdrEncoding::Bc6h {
            mips.push(dds.levels[level].clone());
            continue;
        }
        let mut out = Vec::new();
        for sub in 0..dds.subresources() {
            let (lw, lh, rgb) = dds
                .decode_subresource_rgb(level, sub)
                .map_err(|e| CookError::Dds(format!("dds_bad_header: level {level}: {e}")))?;
            let texels: Vec<[f32; 3]> = rgb.as_chunks::<3>().0.to_vec();
            if encoding == HdrEncoding::AstcHdr {
                out.extend(encode_hdr_blocks(
                    HdrBlockFormat::AstcHdr4x4,
                    lw,
                    lh,
                    &texels,
                    None,
                ));
            } else {
                for t in texels {
                    for v in [t[0], t[1], t[2], 1.0] {
                        let bits = signed_f16_bits(v.clamp(
                            -crate::hdr_import::HDR_MAX_VALUE,
                            crate::hdr_import::HDR_MAX_VALUE,
                        ));
                        out.extend_from_slice(&bits.to_le_bytes());
                    }
                }
            }
        }
        mips.push(out);
    }
    write_ktx2_layered(
        w,
        h,
        Ktx2Layout {
            format,
            block: (block.0, block.1, bpb),
            layers: dds.layers,
            faces: dds.faces,
        },
        &mips,
    )
    .map_err(dds_err)
}

/// ASTC block width and height of an `ASTC_<w>x<h>_*` KTX2 format.
fn astc_block_of(format: ktx2::Format) -> Option<(u32, u32)> {
    let name = format!("{format:?}");
    let dims = name.strip_prefix("ASTC_")?.split('_').next()?;
    let (w, h) = dims.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The KTX2 of a cube map or array of an LDR file, plus the usage it samples as. Blocks pass
/// through on desktop and are transcoded to ASTC per image on Android; the formats with no
/// pass-through (and `compress: false`) are decoded to uncompressed RGBA8.
fn layered_container(
    dds: &DdsImage,
    skip: usize,
    settings: &TextureImporterSettings,
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<(Vec<u8>, TextureCookUsage), CookError> {
    let (w, h) = ((dds.width >> skip).max(1), (dds.height >> skip).max(1));
    let subs = dds.subresources();
    let levels = dds.levels.len();
    if let Some(format) = dds.passthrough_format(usage).filter(|_| settings.compress()) {
        let cooked_usage = passthrough_usage(format);
        let images = |sub: usize| -> Vec<Vec<u8>> {
            (skip..levels)
                .filter_map(|l| dds.subresource(l, sub).map(<[u8]>::to_vec))
                .collect()
        };
        if platform != "android" {
            let mips: Vec<Vec<u8>> = (skip..levels).map(|l| dds.levels[l].clone()).collect();
            let layout = format.layout(dds.layers, dds.faces).map_err(dds_err)?;
            return Ok((
                write_ktx2_layered(w, h, layout, &mips).map_err(dds_err)?,
                cooked_usage,
            ));
        }
        // Android: each image is transcoded as its own 2D chain, then the chains are interleaved.
        let mut per_sub: Vec<Vec<Vec<u8>>> = Vec::with_capacity(subs);
        let mut astc: Option<ktx2::Format> = None;
        for sub in 0..subs {
            let bc = write_ktx2_bcn(w, h, format, &images(sub)).map_err(dds_err)?;
            let blob = crate::texture_compress::transcode_bcn_ktx2_to_astc(
                &bc,
                quality,
                crate::astc_compress::AstcContent::Normal,
            )
            .map_err(dds_err)?;
            let reader = ktx2::Reader::new(&blob).map_err(dds_err)?;
            astc = reader.header().format;
            per_sub.push(reader.levels().map(|l| l.data.to_vec()).collect());
        }
        let format = astc.ok_or_else(|| dds_err("the ASTC transcode left no format"))?;
        let (bw, bh) = astc_block_of(format)
            .ok_or_else(|| dds_err(format!("unexpected transcode format {format:?}")))?;
        let count = per_sub.first().map_or(0, Vec::len);
        let mips: Vec<Vec<u8>> = (0..count)
            .map(|l| {
                per_sub
                    .iter()
                    .flat_map(|chain| chain[l].iter().copied())
                    .collect()
            })
            .collect();
        let layout = Ktx2Layout {
            format,
            block: (bw, bh, 16),
            layers: dds.layers,
            faces: dds.faces,
        };
        return Ok((
            write_ktx2_layered(w, h, layout, &mips).map_err(dds_err)?,
            cooked_usage,
        ));
    }
    // Uncompressed RGBA8, the file's own mip chain (signed BC4/BC5 remapped to unsigned).
    let mut mips = Vec::with_capacity(levels - skip);
    for level in skip..levels {
        let mut out = Vec::new();
        for sub in 0..subs {
            let (_, _, rgba) = dds
                .decode_subresource_rgba8(level, sub)
                .map_err(|e| CookError::Dds(format!("dds_bad_header: level {level}: {e}")))?;
            out.extend_from_slice(&rgba);
        }
        mips.push(out);
    }
    let srgb = usage == TextureCookUsage::ColorSrgb;
    let layout = Ktx2Layout {
        format: if srgb {
            ktx2::Format::R8G8B8A8_SRGB
        } else {
            ktx2::Format::R8G8B8A8_UNORM
        },
        block: (1, 1, 4),
        layers: dds.layers,
        faces: dds.faces,
    };
    Ok((write_ktx2_layered(w, h, layout, &mips).map_err(dds_err)?, usage))
}

/// The default sampling usage of a signed BC4/BC5 source the `.meta` does not name.
fn snorm_usage(format: DdsFormat) -> Option<TextureCookUsage> {
    match format {
        DdsFormat::Bc5Snorm => Some(TextureCookUsage::NormalMap),
        DdsFormat::Bc4Snorm => Some(TextureCookUsage::Mask),
        _ => None,
    }
}

/// The sampling usage: the `.meta` first; a signed BC4/BC5 file is a normal map or a mask; a file
/// that states linear storage is not read as color unless the `.meta` says so.
fn resolve_usage(
    dds: &DdsImage,
    settings: &TextureImporterSettings,
    named: TextureCookUsage,
) -> TextureCookUsage {
    settings.usage_or(match snorm_usage(dds.format) {
        Some(u) => u,
        None if dds.srgb == Some(false) && named == TextureCookUsage::ColorSrgb => {
            TextureCookUsage::ColorLinear
        }
        None => named,
    })
}

/// Cooks a parsed DDS image into its type path and payload.
fn cook_dds_image(
    dds: &DdsImage,
    settings: &TextureImporterSettings,
    named: TextureCookUsage,
    platform: &str,
) -> Result<(String, Vec<u8>), CookError> {
    let usage = resolve_usage(dds, settings, named);
    let quality = settings.quality_or(crate::project_texture_quality::default_texture_quality());
    let skip = levels_to_skip(dds, settings);
    let (w, h) = ((dds.width >> skip).max(1), (dds.height >> skip).max(1));
    let container = |(ktx2, usage): (Vec<u8>, TextureCookUsage)| -> Result<(String, Vec<u8>), CookError> {
        let tex = CookedTexture {
            width: w,
            height: h,
            usage,
            ktx2: ktx2.into(),
        };
        Ok((
            "regia_asset::CookedTexture".to_string(),
            tex.to_postcard().map_err(CookError::Postcard)?,
        ))
    };
    if dds.is_hdr() {
        return container((
            hdr_container(dds, skip, settings, platform)?,
            TextureCookUsage::ColorLinear,
        ));
    }
    if dds.is_layered() {
        return container(layered_container(dds, skip, settings, usage, platform, quality)?);
    }
    let passthrough = dds
        .passthrough_format(usage)
        .filter(|_| dds.levels.len() >= 2 && settings.compress());
    match passthrough {
        Some(format) => {
            let mut ktx2 = write_ktx2_bcn(w, h, format, &dds.levels[skip..]).map_err(dds_err)?;
            if platform == "android" {
                ktx2 = crate::texture_compress::transcode_bcn_ktx2_to_astc(
                    &ktx2,
                    quality,
                    crate::astc_compress::AstcContent::Normal,
                )
                .map_err(dds_err)?;
            }
            container((ktx2, passthrough_usage(format)))
        }
        None => {
            let (width, height, rgba8) = dds
                .decode_level_rgba8(0)
                .map_err(|e| CookError::Dds(format!("dds_bad_header: level 0: {e}")))?;
            crate::cook::cook_rgba8_payload(CookedImage { width, height, rgba8 }, settings, usage, platform)
        }
    }
}

/// Cooks one `.dds` source for `platform` (§ 3 of the spec).
pub(crate) fn cook_dds(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    platform: &str,
) -> Result<DdsCook, CookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "cook_dds", path = %source.display()).entered();
    let meta = AssetDatabase::load_or_create_meta(source, DDS_IMPORTER).map_err(CookError::Db)?;
    let resolved =
        crate::import_rules::resolve_for_cook(project_root, rel_path, DDS_IMPORTER, meta.settings.as_deref());
    let settings =
        TextureImporterSettings::from_meta_string((!resolved.is_empty()).then_some(resolved.as_ref()));
    let dds = parse_dds(bytes).map_err(|e| CookError::Dds(e.to_string()))?;
    let named = usage_from_file_name(source);
    let usage = resolve_usage(&dds, &settings, named);
    let settings_hash =
        *blake3::hash(format!("{};auto={usage:?};dds", settings.fingerprint()).as_bytes()).as_bytes();
    let (type_path, payload) = cook_dds_image(&dds, &settings, named, platform)?;
    Ok(DdsCook {
        settings_hash,
        id: meta.id,
        type_path,
        payload,
    })
}

/// The whole DDS import of `bytes` with default settings, for the fuzz target: parse, then cook
/// for `platform` (`desktop` or `android`), returning the cooked payload. The source is named
/// `fuzz.dds`, so a color file reads as sRGB.
///
/// # Errors
///
/// The error text (it starts with the code) when the file is refused or cannot be cooked.
pub fn import_dds_bytes(bytes: &[u8], platform: &str) -> Result<Vec<u8>, String> {
    let dds = parse_dds(bytes).map_err(|e| e.to_string())?;
    let settings = TextureImporterSettings::from_meta_string(None);
    cook_dds_image(&dds, &settings, TextureCookUsage::ColorSrgb, platform)
        .map(|(_, payload)| payload)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DDS file: FourCC (or `DX10` + `dxgi`) or a legacy RGB layout (`masks`), `levels` raw.
    pub(crate) fn dds_bytes(
        width: u32,
        height: u32,
        fourcc: Option<&[u8; 4]>,
        dxgi: Option<u32>,
        masks: Option<[u32; 4]>,
        levels: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        let put = |h: &mut Vec<u8>, at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut h, 0, HEADER_LEN as u32);
        put(
            &mut h,
            4,
            0x1007 | if levels.len() > 1 { DDSD_MIPMAPCOUNT } else { 0 },
        );
        put(&mut h, 8, height);
        put(&mut h, 12, width);
        put(&mut h, 24, levels.len() as u32);
        put(&mut h, 72, 32);
        if let Some(cc) = fourcc {
            put(&mut h, 76, DDPF_FOURCC);
            h[80..84].copy_from_slice(cc);
        } else if let Some(m) = masks {
            put(
                &mut h,
                76,
                DDPF_RGB | if m[3] != 0 { DDPF_ALPHAPIXELS } else { 0 },
            );
            put(&mut h, 84, 32);
            for (i, mask) in m.iter().enumerate() {
                put(&mut h, 88 + 4 * i, *mask);
            }
        }
        put(&mut h, 104, 0x1000);
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&h);
        if let Some(format) = dxgi {
            for v in [format, D3D10_RESOURCE_DIMENSION_TEXTURE2D, 0, 1, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        for level in levels {
            out.extend_from_slice(level);
        }
        out
    }

    /// BC1 blocks of one solid 5:6:5 color for a `w × h` level.
    pub(crate) fn bc1_solid(w: u32, h: u32, c: u16) -> Vec<u8> {
        let blocks = w.div_ceil(4) * h.div_ceil(4);
        let mut out = Vec::new();
        for _ in 0..blocks {
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        out
    }

    fn chain(w: u32, h: u32, f: impl Fn(u32, u32) -> Vec<u8>) -> Vec<Vec<u8>> {
        let n = 32 - w.max(h).leading_zeros();
        (0..n).map(|i| f((w >> i).max(1), (h >> i).max(1))).collect()
    }

    #[test]
    fn bc1_fourcc_with_a_full_chain_parses() {
        let levels = chain(8, 4, |w, h| bc1_solid(w, h, 0xF800));
        let dds = parse_dds(&dds_bytes(8, 4, Some(b"DXT1"), None, None, &levels)).expect("parse");
        assert_eq!(
            (dds.width, dds.height, dds.format, dds.srgb),
            (8, 4, DdsFormat::Bc1, None)
        );
        assert_eq!(dds.levels, levels);
        let (w, h, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!((w, h), (8, 4));
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn bc1_punch_through_alpha_is_decoded_not_passed_through() {
        // c0 = c1 (three-color mode) and texel 0 at index 3: transparent black.
        let mut block = Vec::new();
        block.extend_from_slice(&0x001Fu16.to_le_bytes());
        block.extend_from_slice(&0xF800u16.to_le_bytes());
        block.extend_from_slice(&3u32.to_le_bytes());
        let levels = vec![block.clone(), block.clone(), block];
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DXT1"), None, None, &levels)).expect("parse");
        assert!(dds.has_punch_through_alpha());
        assert_eq!(dds.passthrough_format(TextureCookUsage::ColorSrgb), None);
        let (_, _, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!(rgba[3], 0, "texel 0 is transparent");
        let opaque = chain(4, 4, |w, h| bc1_solid(w, h, 0xF800));
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DXT1"), None, None, &opaque)).expect("parse");
        assert!(!dds.has_punch_through_alpha(), "c0 = c1 with index 0 only");
    }

    #[test]
    fn dx10_formats_carry_their_color_space() {
        let levels = chain(4, 4, |w, h| vec![7u8; DdsFormat::Bc7.level_len(w, h)]);
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DX10"), Some(99), None, &levels)).expect("parse");
        assert_eq!((dds.format, dds.srgb), (DdsFormat::Bc7, Some(true)));
        assert_eq!(
            dds.passthrough_format(TextureCookUsage::ColorLinear),
            Some(BcnFormat::Bc7Srgb)
        );
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DX10"), Some(98), None, &levels)).expect("parse");
        assert_eq!(
            dds.passthrough_format(TextureCookUsage::ColorSrgb),
            Some(BcnFormat::Bc7Linear)
        );
        // A legacy FourCC says nothing: the usage decides.
        let bc3 = chain(4, 4, |w, h| vec![0u8; DdsFormat::Bc3.level_len(w, h)]);
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DXT5"), None, None, &bc3)).expect("parse");
        assert_eq!(
            dds.passthrough_format(TextureCookUsage::ColorSrgb),
            Some(BcnFormat::Bc3Srgb)
        );
        assert_eq!(
            dds.passthrough_format(TextureCookUsage::ColorLinear),
            Some(BcnFormat::Bc3Linear)
        );
    }

    #[test]
    fn odd_sizes_and_partial_chains() {
        // 5×3: level 0 is 2×1 blocks, level 1 (2×1) one block; two of three levels present.
        let levels = vec![bc1_solid(5, 3, 0x07E0), bc1_solid(2, 1, 0x07E0)];
        assert_eq!(levels[0].len(), 16);
        let dds = parse_dds(&dds_bytes(5, 3, Some(b"DXT1"), None, None, &levels)).expect("parse");
        assert_eq!(dds.levels.len(), 2);
        let (w, h, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!((w, h, rgba.len()), (5, 3, 60));
        assert_eq!(&rgba[56..60], &[0, 255, 0, 255]);
    }

    #[test]
    fn bc2_decodes_explicit_alpha() {
        // Red (index 0) everywhere, alpha nibbles 0..15 across the 16 texels.
        let mut block = Vec::new();
        let alpha: u64 = (0..16u64).fold(0, |acc, t| acc | (t << (4 * t)));
        block.extend_from_slice(&alpha.to_le_bytes());
        block.extend_from_slice(&0xF800u16.to_le_bytes());
        block.extend_from_slice(&0x001Fu16.to_le_bytes());
        block.extend_from_slice(&0x0000_0004u32.to_le_bytes()); // texel 1 = index 1 (blue)
        let dds = parse_dds(&dds_bytes(4, 4, Some(b"DXT3"), None, None, &[block])).expect("parse");
        assert_eq!(dds.format, DdsFormat::Bc2);
        assert_eq!(dds.passthrough_format(TextureCookUsage::ColorSrgb), None);
        let (_, _, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!(&rgba[0..4], &[255, 0, 0, 0]);
        assert_eq!(&rgba[4..8], &[0, 0, 255, 17]);
        assert_eq!(&rgba[60..64], &[255, 0, 0, 255]);
    }

    #[test]
    fn bgra_and_legacy_masks_are_swizzled() {
        let px = vec![0x30, 0x20, 0x10, 0x40]; // B G R A in memory
        let dds = parse_dds(&dds_bytes(
            1,
            1,
            Some(b"DX10"),
            Some(87),
            None,
            std::slice::from_ref(&px),
        ))
        .expect("parse");
        assert_eq!(dds.levels[0], vec![0x10, 0x20, 0x30, 0x40]);
        let legacy = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0];
        let dds = parse_dds(&dds_bytes(1, 1, None, None, Some(legacy), &[px])).expect("parse");
        assert_eq!(
            dds.levels[0],
            vec![0x10, 0x20, 0x30, 0xFF],
            "no alpha mask reads opaque"
        );
        let rgba = [0xFF, 0xFF00, 0x00FF_0000, 0xFF00_0000];
        let dds = parse_dds(&dds_bytes(1, 1, None, None, Some(rgba), &[vec![1, 2, 3, 4]])).expect("parse");
        assert_eq!(dds.levels[0], vec![1, 2, 3, 4]);
    }

    #[test]
    fn refusals_carry_their_codes() {
        let one = vec![bc1_solid(4, 4, 0)];
        assert_eq!(
            parse_dds(b"PNG...").map(|_| ()).expect_err("refused").code(),
            "dds_bad_header"
        );
        let mut cube = dds_bytes(4, 4, Some(b"DXT1"), None, None, &one);
        cube[4 + 108..4 + 112].copy_from_slice(&DDSCAPS2_CUBEMAP.to_le_bytes());
        let err = parse_dds(&cube).expect_err("a cube without its six faces is refused");
        assert_eq!(err.code(), "dds_unsupported");
        assert!(err.to_string().contains("six faces"), "{err}");
        let mut volume = dds_bytes(4, 4, Some(b"DXT1"), None, None, &one);
        volume[4 + 108..4 + 112].copy_from_slice(&DDSCAPS2_VOLUME.to_le_bytes());
        let err = parse_dds(&volume).expect_err("refused");
        assert_eq!(err.code(), "dds_unsupported");
        assert!(err.to_string().contains("no cooked form"), "{err}");
        let bytes = dds_bytes(4, 4, Some(b"DXT1"), None, None, &one);
        assert!(matches!(
            parse_dds(&bytes[..bytes.len() - 1]),
            Err(DdsError::Truncated {
                level: 0,
                needed: 8,
                available: 7
            })
        ));
        assert_eq!(
            parse_dds(&dds_bytes(4, 4, Some(b"ABCD"), None, None, &one))
                .expect_err("refused")
                .code(),
            "dds_unsupported"
        );
        // More levels than the size allows.
        let many = vec![bc1_solid(4, 4, 0); 4];
        assert_eq!(
            parse_dds(&dds_bytes(4, 4, Some(b"DXT1"), None, None, &many))
                .expect_err("refused")
                .code(),
            "dds_bad_header"
        );
    }

    /// A DDS with explicit caps2, DX10 misc flag, array size and one `levels` chain per image
    /// (array element major, then face).
    fn dds_layered(
        size: u32,
        fourcc: Option<&[u8; 4]>,
        dxgi: Option<u32>,
        caps2: u32,
        dx10: (u32, u32, u32),
        images: &[Vec<Vec<u8>>],
    ) -> Vec<u8> {
        let mut out = dds_bytes(size, size, fourcc, dxgi, None, &images[0]);
        out[4 + 108..4 + 112].copy_from_slice(&caps2.to_le_bytes());
        out.truncate(4 + HEADER_LEN + if dxgi.is_some() { DX10_LEN } else { 0 });
        if dxgi.is_some() {
            let at = 4 + HEADER_LEN;
            out[at + 4..at + 8].copy_from_slice(&dx10.0.to_le_bytes());
            out[at + 8..at + 12].copy_from_slice(&dx10.1.to_le_bytes());
            out[at + 12..at + 16].copy_from_slice(&dx10.2.to_le_bytes());
        }
        for image in images {
            for level in image {
                out.extend_from_slice(level);
            }
        }
        out
    }

    const ALL_FACES: u32 = DDSCAPS2_CUBEMAP | DDSCAPS2_CUBEMAP_ALL_FACES;

    fn solid_cube(size: u32, count: usize) -> Vec<Vec<Vec<u8>>> {
        (0..count)
            .map(|i| chain(size, size, |w, h| bc1_solid(w, h, 0x0841 * (i as u16 + 1))))
            .collect()
    }

    fn ktx2_of(payload: &[u8]) -> (CookedTexture, Vec<u8>) {
        let tex = CookedTexture::from_postcard(payload).expect("cooked texture");
        let bytes = tex.ktx2.to_vec();
        (tex, bytes)
    }

    #[test]
    fn dx9_cube_keeps_six_faces_and_the_chain() {
        let images = solid_cube(8, 6);
        let bytes = dds_layered(8, Some(b"DXT1"), None, ALL_FACES, (0, 0, 0), &images);
        let dds = parse_dds(&bytes).expect("parse");
        assert_eq!((dds.faces, dds.layers, dds.levels.len()), (6, 0, 4));
        // Level 0 holds the six 8×8 faces (4 blocks of 8 bytes each), in file order.
        assert_eq!(dds.levels[0].len(), 6 * 4 * 8);
        assert_eq!(dds.subresource(0, 5), Some(&images[5][0][..]));
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop").expect("cook");
        let (tex, ktx2_bytes) = ktx2_of(&payload);
        let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
        let h = reader.header();
        assert_eq!((h.face_count, h.layer_count, h.level_count), (6, 0, 4));
        assert_eq!(h.format, Some(ktx2::Format::BC1_RGB_SRGB_BLOCK));
        assert_eq!((tex.width, tex.height), (8, 8));
        let level0 = reader.levels().next().expect("level 0").data;
        assert_eq!(
            &level0[3 * 32..4 * 32],
            &images[3][0][..],
            "face 3 is the fourth face"
        );
    }

    #[test]
    fn dx10_cube_matches_the_dx9_cube() {
        let images = solid_cube(8, 6);
        let dx9 = parse_dds(&dds_layered(
            8,
            Some(b"DXT1"),
            None,
            ALL_FACES,
            (0, 0, 0),
            &images,
        ))
        .expect("dx9");
        let dx10 = dds_layered(
            8,
            Some(b"DX10"),
            Some(71),
            0,
            (
                D3D10_RESOURCE_DIMENSION_TEXTURE2D,
                D3D10_RESOURCE_MISC_TEXTURECUBE,
                1,
            ),
            &images,
        );
        let dx10 = parse_dds(&dx10).expect("dx10");
        assert_eq!((dx10.faces, dx10.layers), (6, 0));
        assert_eq!(dx10.levels, dx9.levels);
    }

    #[test]
    fn cube_array_has_cubes_times_six_images() {
        let images = solid_cube(4, 12);
        let bytes = dds_layered(
            4,
            Some(b"DX10"),
            Some(71),
            0,
            (
                D3D10_RESOURCE_DIMENSION_TEXTURE2D,
                D3D10_RESOURCE_MISC_TEXTURECUBE,
                2,
            ),
            &images,
        );
        let dds = parse_dds(&bytes).expect("parse");
        assert_eq!((dds.faces, dds.layers, dds.subresources()), (6, 2, 12));
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop").expect("cook");
        let (_, ktx2_bytes) = ktx2_of(&payload);
        let h = ktx2::Reader::new(&ktx2_bytes).expect("ktx2").header();
        assert_eq!((h.face_count, h.layer_count), (6, 2));
    }

    #[test]
    fn array_of_three_cooks_three_layers() {
        let images = solid_cube(8, 3);
        let bytes = dds_layered(
            8,
            Some(b"DX10"),
            Some(72),
            0,
            (D3D10_RESOURCE_DIMENSION_TEXTURE2D, 0, 3),
            &images,
        );
        let dds = parse_dds(&bytes).expect("parse");
        assert_eq!((dds.faces, dds.layers), (1, 3));
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop").expect("cook");
        let (_, ktx2_bytes) = ktx2_of(&payload);
        let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
        let h = reader.header();
        assert_eq!((h.face_count, h.layer_count, h.level_count), (1, 3, 4));
        assert_eq!(
            h.format,
            Some(ktx2::Format::BC1_RGB_SRGB_BLOCK),
            "the DX10 format says sRGB"
        );
        let level2 = reader.levels().nth(2).expect("level 2").data;
        assert_eq!(level2.len(), 3 * 8, "one block a layer at 2×2");
        assert_eq!(&level2[16..24], &images[2][2][..]);
        // One element is a plain texture.
        let one = dds_layered(
            8,
            Some(b"DX10"),
            Some(72),
            0,
            (D3D10_RESOURCE_DIMENSION_TEXTURE2D, 0, 1),
            &images[..1],
        );
        assert_eq!(parse_dds(&one).expect("parse").layers, 0);
    }

    #[test]
    fn volumes_are_refused_with_the_reason() {
        let images = solid_cube(4, 1);
        let bytes = dds_layered(
            4,
            Some(b"DX10"),
            Some(71),
            0,
            (D3D10_RESOURCE_DIMENSION_TEXTURE3D, 0, 1),
            &images,
        );
        let err = parse_dds(&bytes).expect_err("refused");
        assert_eq!(err.code(), "dds_unsupported");
        assert!(err.to_string().contains("volume"), "{err}");
    }

    #[test]
    fn truncated_cube_reports_the_missing_data() {
        let images = solid_cube(8, 6);
        let bytes = dds_layered(8, Some(b"DXT1"), None, ALL_FACES, (0, 0, 0), &images);
        let err = parse_dds(&bytes[..bytes.len() - 1]).expect_err("short");
        assert_eq!(err.code(), "dds_truncated");
    }

    #[test]
    fn android_cube_is_transcoded_to_astc_per_face() {
        let images = solid_cube(8, 6);
        let bytes = dds_layered(8, Some(b"DXT1"), None, ALL_FACES, (0, 0, 0), &images);
        let dds = parse_dds(&bytes).expect("parse");
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "android").expect("cook");
        let (_, ktx2_bytes) = ktx2_of(&payload);
        let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
        let h = reader.header();
        assert_eq!((h.face_count, h.level_count), (6, 4));
        let name = format!("{:?}", h.format.expect("format"));
        assert!(name.starts_with("ASTC_"), "{name}");
    }

    /// A BC6H block from the encoder, solid `value` in the given variant.
    fn bc6h_solid(value: f32, signed: bool) -> Vec<u8> {
        use crate::bc6h_encode::{Bc6hFormat, Bc6hOptions, encode_block};
        let format = if signed {
            Bc6hFormat::Signed
        } else {
            Bc6hFormat::Unsigned
        };
        encode_block(&[[value; 3]; 16], format, &Bc6hOptions::default()).to_vec()
    }

    #[test]
    fn bc6h_blocks_decode_to_their_values_signed_and_unsigned() {
        // DXGI 95 is BC6H_UF16, 96 is BC6H_SF16.
        let uf = parse_dds(&dds_bytes(
            4,
            4,
            Some(b"DX10"),
            Some(95),
            None,
            &[bc6h_solid(0.25, false)],
        ))
        .expect("uf16");
        assert_eq!(uf.format, DdsFormat::Bc6hUf16);
        let (_, _, rgb) = uf.decode_subresource_rgb(0, 0).expect("decode");
        assert!(
            rgb.iter().all(|v| (v - 0.25).abs() < 0.25 * 0.02),
            "{:?}",
            &rgb[..3]
        );
        let sf = parse_dds(&dds_bytes(
            4,
            4,
            Some(b"DX10"),
            Some(96),
            None,
            &[bc6h_solid(-0.5, true)],
        ))
        .expect("sf16");
        assert_eq!(sf.format, DdsFormat::Bc6hSf16);
        let (_, _, rgb) = sf.decode_subresource_rgb(0, 0).expect("decode");
        assert!(
            rgb.iter().all(|v| (v + 0.5).abs() < 0.5 * 0.02),
            "{:?}",
            &rgb[..3]
        );
        // A zero block (mode 1 with all-zero fields) decodes to black.
        let zero =
            parse_dds(&dds_bytes(4, 4, Some(b"DX10"), Some(95), None, &[vec![0u8; 16]])).expect("zero");
        let (_, _, rgb) = zero.decode_subresource_rgb(0, 0).expect("decode");
        assert!(rgb.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn bc6h_cooks_passthrough_on_desktop_and_float_on_android() {
        for (dxgi, signed, vk) in [
            (95u32, false, ktx2::Format::BC6H_UFLOAT_BLOCK),
            (96, true, ktx2::Format::BC6H_SFLOAT_BLOCK),
        ] {
            let level = bc6h_solid(if signed { -0.5 } else { 0.5 }, signed);
            let bytes = dds_bytes(
                4,
                4,
                Some(b"DX10"),
                Some(dxgi),
                None,
                std::slice::from_ref(&level),
            );
            let dds = parse_dds(&bytes).expect("parse");
            let (_, payload) =
                cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop")
                    .expect("cook");
            let (tex, ktx2_bytes) = ktx2_of(&payload);
            let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
            assert_eq!(reader.header().format, Some(vk));
            assert_eq!(
                reader.levels().next().expect("level").data,
                &level[..],
                "blocks pass through"
            );
            assert_eq!(tex.usage, TextureCookUsage::ColorLinear);
            // Android: RGBA16F with the sign kept.
            let (_, payload) =
                cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "android")
                    .expect("cook");
            let (_, ktx2_bytes) = ktx2_of(&payload);
            let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
            assert_eq!(reader.header().format, Some(ktx2::Format::R16G16B16A16_SFLOAT));
            let texel = &reader.levels().next().expect("level").data[..8];
            let r = f16_to_f32(u16::from_le_bytes([texel[0], texel[1]]));
            let want = if signed { -0.5 } else { 0.5 };
            assert!((r - want).abs() < 0.02, "{r} vs {want}");
        }
    }

    /// A half float with its sign (`f16_bits_to_f32` reads only the non-negative range).
    fn f16_to_f32(bits: u16) -> f32 {
        let magnitude = regia_asset::hdr_blocks::f16_bits_to_f32(bits & 0x7FFF);
        if bits & 0x8000 != 0 { -magnitude } else { magnitude }
    }

    #[test]
    fn bc6h_cube_with_astc_hdr_setting_on_android() {
        let images: Vec<Vec<Vec<u8>>> = (0..6).map(|_| vec![bc6h_solid(1.0, false)]).collect();
        let bytes = dds_layered(
            4,
            Some(b"DX10"),
            Some(95),
            0,
            (
                D3D10_RESOURCE_DIMENSION_TEXTURE2D,
                D3D10_RESOURCE_MISC_TEXTURECUBE,
                1,
            ),
            &images,
        );
        let dds = parse_dds(&bytes).expect("parse");
        let settings = TextureImporterSettings::from_meta_string(Some("(hdr_encoding: Some(astc_hdr))"));
        let (_, payload) =
            cook_dds_image(&dds, &settings, TextureCookUsage::ColorSrgb, "android").expect("cook");
        let (_, ktx2_bytes) = ktx2_of(&payload);
        let h = ktx2::Reader::new(&ktx2_bytes).expect("ktx2").header();
        assert_eq!(
            (h.face_count, h.format),
            (6, Some(ktx2::Format::ASTC_4x4_SFLOAT_BLOCK))
        );
    }

    /// A BC5 SNORM block with a constant X and Y (endpoints equal, every index 0).
    fn bc5_snorm_solid(x: i8, y: i8) -> Vec<u8> {
        let mut b = vec![x as u8, x as u8, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&[y as u8, y as u8, 0, 0, 0, 0, 0, 0]);
        b
    }

    #[test]
    fn bc5_snorm_decodes_signed_values_and_rebuilds_z() {
        let dds = parse_dds(&dds_bytes(
            4,
            4,
            Some(b"DX10"),
            Some(84),
            None,
            &[bc5_snorm_solid(0, 0)],
        ))
        .expect("parse");
        assert_eq!(dds.format, DdsFormat::Bc5Snorm);
        let (_, _, rgba) = dds.decode_level_rgba8(0).expect("decode");
        // (0, 0) remaps to 128 (0.5), and Z rebuilds to 1 → 255.
        assert_eq!(&rgba[..4], &[128, 128, 255, 255]);
        let dds = parse_dds(&dds_bytes(
            4,
            4,
            Some(b"DX10"),
            Some(84),
            None,
            &[bc5_snorm_solid(127, -127)],
        ))
        .expect("parse");
        let (_, _, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!(&rgba[..3], &[255, 0, 128], "x = 1, y = -1, z = 0");
        // -128 reads as -127.
        let dds = parse_dds(&dds_bytes(
            4,
            4,
            Some(b"DX10"),
            Some(84),
            None,
            &[bc5_snorm_solid(-128, -127)],
        ))
        .expect("parse");
        let (_, _, rgba) = dds.decode_level_rgba8(0).expect("decode");
        assert_eq!(&rgba[..2], &[0, 0]);
    }

    #[test]
    fn bc4_snorm_interpolates_with_both_palette_modes() {
        // r0 > r1: eight values; index 2 is (6 r0 + r1) / 7.
        let p = bc4_snorm_palette(127, -127);
        assert!((p[2] - (6.0 * 1.0 - 1.0) / 7.0).abs() < 1e-6);
        // r0 <= r1: four interpolants, then -1 and 1.
        let p = bc4_snorm_palette(-127, 127);
        assert!((p[2] - (-4.0 + 1.0) / 5.0).abs() < 1e-6);
        assert_eq!((p[6], p[7]), (-1.0, 1.0));
    }

    #[test]
    fn snorm_normal_map_cooks_to_unorm_bc5_and_its_chain_survives() {
        let levels = chain(8, 8, |w, h| {
            (0..w.div_ceil(4) * h.div_ceil(4))
                .flat_map(|_| bc5_snorm_solid(0, 0))
                .collect()
        });
        let dds = parse_dds(&dds_bytes(8, 8, Some(b"DX10"), Some(84), None, &levels)).expect("parse");
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop").expect("cook");
        let (tex, ktx2_bytes) = ktx2_of(&payload);
        assert_eq!(tex.usage, TextureCookUsage::NormalMap);
        let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::BC5_UNORM_BLOCK));
        assert!(reader.levels().count() > 1, "the cook generates a mip chain");
    }

    #[test]
    fn rgba8_cube_is_stored_uncompressed() {
        let face = |v: u8| vec![[v, 0, 0, 255].repeat(4)];
        let images: Vec<Vec<Vec<u8>>> = (0..6).map(|i| face(i * 40)).collect();
        let bytes = dds_layered_masked(2, 2, &images);
        let dds = parse_dds(&bytes).expect("parse");
        assert_eq!((dds.faces, dds.format), (6, DdsFormat::Rgba8));
        let (_, payload) =
            cook_dds_image(&dds, &Default::default(), TextureCookUsage::ColorSrgb, "desktop").expect("cook");
        let (_, ktx2_bytes) = ktx2_of(&payload);
        let reader = ktx2::Reader::new(&ktx2_bytes).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::R8G8B8A8_SRGB));
        assert_eq!(reader.levels().next().expect("level").data.len(), 6 * 16);
    }

    /// A legacy 32-bit RGBA cube (all six faces) of `size × size` with one level.
    fn dds_layered_masked(size: u32, _unused: u32, images: &[Vec<Vec<u8>>]) -> Vec<u8> {
        let rgba = [0xFF, 0xFF00, 0x00FF_0000, 0xFF00_0000];
        let mut out = dds_bytes(size, size, None, None, Some(rgba), &images[0]);
        out[4 + 108..4 + 112].copy_from_slice(&ALL_FACES.to_le_bytes());
        out.truncate(4 + HEADER_LEN);
        for image in images {
            out.extend_from_slice(&image[0]);
        }
        out
    }

    #[test]
    fn fuzz_fixture_imports_on_both_platforms() {
        let bytes = crate::fuzz_entry::dds_fixture();
        assert!(crate::fuzz_entry::dds_load(&bytes).is_ok());
    }

    /// One 4×4 BC3 block of a single RGB565 color `c` and a single alpha (all indices 0):
    /// the 8-byte alpha block first, then the 8-byte color block, as DXT5 stores them.
    fn bc3_solid(c: u16, alpha: u8) -> Vec<u8> {
        let mut out = vec![alpha, alpha];
        out.extend_from_slice(&[0u8; 6]);
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    #[test]
    fn dxt4_is_premultiplied_and_decodes_to_straight_color() {
        // 50% white: DXT4 stores the color premultiplied (≈ half color at alpha 128), DXT5 stores
        // it straight (full color at alpha 128). Both must cook to the same straight texels.
        // 0x8410 decodes to RGB565 (132, 130, 132): half of white at alpha 128.
        let premult = chain(4, 4, |_, _| bc3_solid(0x8410, 128));
        let dxt4 = parse_dds(&dds_bytes(4, 4, Some(b"DXT4"), None, None, &premult)).expect("parse");
        assert!(dxt4.premultiplied);
        assert_eq!(
            dxt4.passthrough_format(TextureCookUsage::ColorSrgb),
            None,
            "premultiplied blocks never pass through"
        );
        let (_, _, dxt4_rgba) = dxt4.decode_level_rgba8(0).expect("decode");
        let straight = chain(4, 4, |_, _| bc3_solid(0xFFFF, 128));
        let dxt5 = parse_dds(&dds_bytes(4, 4, Some(b"DXT5"), None, None, &straight)).expect("parse");
        assert!(!dxt5.premultiplied);
        let (_, _, dxt5_rgba) = dxt5.decode_level_rgba8(0).expect("decode");
        assert_eq!(&dxt5_rgba[..4], &[255, 255, 255, 128]);
        assert_eq!(dxt4_rgba, dxt5_rgba, "both cook to straight white at alpha 128");
        // The same stored block read as DXT5 stays the dark straight color (the old DXT4 bug).
        let dark = parse_dds(&dds_bytes(4, 4, Some(b"DXT5"), None, None, &premult)).expect("parse");
        let (_, _, dark_rgba) = dark.decode_level_rgba8(0).expect("decode");
        assert!(
            dark_rgba[0] < 200,
            "stored premultiplied blocks stay dark when read as straight: {dark_rgba:?}"
        );
    }

    #[test]
    fn dxt2_is_premultiplied_and_decodes_to_straight_color() {
        // BC2: 8 bytes of 4-bit alpha (nibble 8 → 8 × 17 = 136) then a BC1 color block.
        let mut block = [0x88u8; 8].to_vec();
        block.extend_from_slice(&0x8C51u16.to_le_bytes());
        block.extend_from_slice(&0x8C51u16.to_le_bytes());
        block.extend_from_slice(&0u32.to_le_bytes());
        let levels = vec![block.clone(), block];
        let dxt2 = parse_dds(&dds_bytes(4, 4, Some(b"DXT2"), None, None, &levels)).expect("parse");
        assert!(dxt2.premultiplied);
        let (_, _, rgba) = dxt2.decode_level_rgba8(0).expect("decode");
        for px in rgba.as_chunks::<4>().0 {
            assert_eq!(px[3], 136);
            assert!(
                px[..3].iter().all(|&c| c >= 254),
                "premultiplied color unpremultiplies to white: {px:?}"
            );
        }
        let dxt3 = parse_dds(&dds_bytes(4, 4, Some(b"DXT3"), None, None, &levels)).expect("parse");
        assert!(!dxt3.premultiplied);
        let (_, _, straight) = dxt3.decode_level_rgba8(0).expect("decode");
        for px in straight.as_chunks::<4>().0 {
            assert!(px[..3].iter().all(|&c| (120..=150).contains(&c)));
        }
    }

    /// Spec 95 § 3 as amended by ADR-3266: the DX10 header's cube flag and an array size above one
    /// are read (a file holding one image under them is short of its other faces or layers), while a
    /// DX10 resource that is not 2D, a missing DX10 header and a 24-bit legacy layout are refused.
    #[test]
    fn dx10_cube_and_array_fields_are_read_and_other_dimensions_refused() {
        let one = vec![bc1_solid(4, 4, 0)];
        let base = dds_bytes(4, 4, Some(b"DX10"), Some(71), None, &one);
        assert!(parse_dds(&base).is_ok(), "the unmodified DX10 BC1 file parses");
        // DX10 header layout after the 128-byte prefix: dxgi, dimension, misc flag, array size.
        let dx10 = 4 + HEADER_LEN;
        let with = |at: usize, value: u32| {
            let mut bytes = base.clone();
            bytes[dx10 + at..dx10 + at + 4].copy_from_slice(&value.to_le_bytes());
            bytes
        };
        // ADR-3266: cubes and arrays import, so the flag and the size are honoured and the single
        // stored image is not enough.
        for (what, bytes) in [
            ("cube flag", with(8, D3D10_RESOURCE_MISC_TEXTURECUBE)),
            ("array of two", with(12, 2)),
        ] {
            let err = parse_dds(&bytes).expect_err(what);
            assert_eq!(err.code(), "dds_truncated", "{what}: {err}");
        }
        let err = parse_dds(&with(4, D3D10_RESOURCE_DIMENSION_TEXTURE2D + 1)).expect_err("3D resource");
        assert_eq!(err.code(), "dds_unsupported", "3D resource: {err}");
        // An array size of 0 is read as one texture (the header's own convention for "not an array").
        assert!(parse_dds(&with(12, 0)).is_ok());
        let cut = &base[..dx10 + 8];
        assert_eq!(
            parse_dds(cut).expect_err("no DX10 header").code(),
            "dds_bad_header"
        );
        let mut rgb24 = dds_bytes(
            1,
            1,
            None,
            None,
            Some([0xFF_0000, 0xFF00, 0xFF, 0]),
            &[vec![0; 4]],
        );
        rgb24[4 + 84..4 + 88].copy_from_slice(&24u32.to_le_bytes());
        assert_eq!(parse_dds(&rgb24).expect_err("24-bit").code(), "dds_unsupported");
    }

    /// Spec 95 § 3: every accepted FourCC alias and the DX10 BC1/BC2/BC3/BC4/BC5 formats parse to
    /// the right layout, and the pass-through formats are the ones the spec names.
    #[test]
    fn every_accepted_fourcc_and_dx10_format_parses() {
        let blocks = |format: DdsFormat| chain(8, 8, move |w, h| vec![0u8; format.level_len(w, h)]);
        for (cc, format) in [
            (b"DXT1", DdsFormat::Bc1),
            (b"DXT2", DdsFormat::Bc2),
            (b"DXT3", DdsFormat::Bc2),
            (b"DXT4", DdsFormat::Bc3),
            (b"DXT5", DdsFormat::Bc3),
            (b"ATI1", DdsFormat::Bc4),
            (b"BC4U", DdsFormat::Bc4),
            (b"ATI2", DdsFormat::Bc5),
            (b"BC5U", DdsFormat::Bc5),
        ] {
            let levels = blocks(format);
            let dds = parse_dds(&dds_bytes(8, 8, Some(cc), None, None, &levels))
                .unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(cc)));
            assert_eq!(dds.format, format, "{}", String::from_utf8_lossy(cc));
            assert_eq!(dds.levels, levels);
        }
        for (dxgi, format, srgb) in [
            (70, DdsFormat::Bc1, None),
            (71, DdsFormat::Bc1, Some(false)),
            (72, DdsFormat::Bc1, Some(true)),
            (74, DdsFormat::Bc2, Some(false)),
            (76, DdsFormat::Bc3, None),
            (77, DdsFormat::Bc3, Some(false)),
            (78, DdsFormat::Bc3, Some(true)),
            (80, DdsFormat::Bc4, Some(false)),
            (83, DdsFormat::Bc5, Some(false)),
            (97, DdsFormat::Bc7, None),
        ] {
            let levels = blocks(format);
            let dds = parse_dds(&dds_bytes(8, 8, Some(b"DX10"), Some(dxgi), None, &levels))
                .unwrap_or_else(|e| panic!("DXGI {dxgi}: {e}"));
            assert_eq!((dds.format, dds.srgb), (format, srgb), "DXGI {dxgi}");
        }
        // Signed BC4/BC5 under their FourCCs are refused like every unknown FourCC.
        for cc in [b"BC4S", b"BC5S"] {
            let levels = blocks(DdsFormat::Bc4);
            let err = parse_dds(&dds_bytes(8, 8, Some(cc), None, None, &levels)).expect_err("signed");
            assert_eq!(err.code(), "dds_unsupported");
        }
        // BC4 and BC5 pass through whatever the usage says; BC2 never does.
        let bc4 = parse_dds(&dds_bytes(
            8,
            8,
            Some(b"ATI1"),
            None,
            None,
            &blocks(DdsFormat::Bc4),
        ))
        .expect("bc4");
        assert_eq!(
            bc4.passthrough_format(TextureCookUsage::ColorSrgb),
            Some(BcnFormat::Bc4)
        );
        assert_eq!(passthrough_usage(BcnFormat::Bc4), TextureCookUsage::Mask);
        let bc5 = parse_dds(&dds_bytes(
            8,
            8,
            Some(b"ATI2"),
            None,
            None,
            &blocks(DdsFormat::Bc5),
        ))
        .expect("bc5");
        assert_eq!(
            bc5.passthrough_format(TextureCookUsage::ColorSrgb),
            Some(BcnFormat::Bc5)
        );
        assert_eq!(passthrough_usage(BcnFormat::Bc5), TextureCookUsage::NormalMap);
    }
}
