// SPDX-License-Identifier: MPL-2.0
//! The HDR image importer (`docs/specs/12-assets.md` § 7.1, ADR-2546): `.exr` (OpenEXR) and
//! `.hdr` (Radiance RGBE) sources cook to a KTX2 holding linear float texels with a full mip
//! chain.
//!
//! - **Decode and validate** ([`decode_hdr_source`]): the `image` crate reads either container to
//!   RGBA `f32`. NaN and infinite samples become 0 and are counted as one cook warning per file;
//!   negative samples clamp to 0; samples above the largest finite half float (65504) clamp to
//!   it. Alpha clamps to `[0, 1]`.
//! - **Encode** ([`HdrEncoding`]): BC6H unsigned float on desktop (the one-region modes, which
//!   `regia_asset::hdr_texture` reads back on the CPU), uncompressed RGBA16F on Android, or the
//!   `.meta` `hdr_encoding` / `compress` choice. Every platform gets the whole mip chain, each
//!   level box-filtered from the one above in linear float.
//! - **Cube map** (`cube_face_px` in the `.meta`): an equirect source is converted at cook to
//!   six faces ([`equirect_to_cube`]). Every cube texel averages a grid of bilinear equirect
//!   samples over its own footprint, so faces meet without seams (the equirect wraps in azimuth
//!   and clamps at the poles) and the grid keeps a small sun from aliasing.
//!
//! Original code from the public OpenEXR and Radiance RGBE container descriptions (through the
//! `image` crate), the Khronos KTX 2.0 specification and the Vulkan specification's cube map
//! conventions (`docs/provenance.md`).

use std::path::Path;

use regia_asset::hdr_blocks::{HdrBlockFormat, encode_hdr_blocks, f32_to_f16_bits};
use regia_asset::hdr_texture::{HdrImage, cube_face_direction};
use regia_asset::{CookedTexture, TextureCookUsage};

use crate::bc6h_encode::{Bc6hFormat, Bc6hMetric, Bc6hModes, Bc6hOptions, encode_image};
use crate::cook::CookError;
use crate::db::AssetDatabase;
use crate::texture_settings::{HdrEncodingSetting, TextureImporterSettings};

/// The importer name the cook records for HDR sources.
pub const HDR_IMPORTER: &str = "hdr";

/// The largest source, texels (a 8192 × 4096 sky is a third of it).
pub const HDR_MAX_TEXELS: u64 = 1 << 26;
/// The largest finite half float: the brightest value a cooked texel keeps.
pub const HDR_MAX_VALUE: f32 = 65_504.0;
/// Smallest and largest `cube_face_px`.
pub const CUBE_FACE_PX_RANGE: std::ops::RangeInclusive<u32> = 4..=8192;
/// The relative per-channel error a BC6H texel of a smooth sky stays within (the bound the cook's
/// tests check; see ADR-2546).
pub const HDR_BC6H_MAX_REL_ERROR: f32 = 0.08;

/// How the cooked texels are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrEncoding {
    /// BC6H unsigned float, one-region modes.
    Bc6h,
    /// Uncompressed RGBA16F.
    Rgba16f,
    /// ASTC 4×4 HDR.
    AstcHdr,
}

impl HdrEncoding {
    /// The encoding `settings` ask for on `platform` (`android`, `linux`, `windows`, …).
    #[must_use]
    pub fn choose(settings: &TextureImporterSettings, platform: &str) -> Self {
        if !settings.compress() {
            return Self::Rgba16f;
        }
        match settings.hdr_encoding.unwrap_or(HdrEncodingSetting::Auto) {
            HdrEncodingSetting::Bc6h => Self::Bc6h,
            HdrEncodingSetting::Rgba16f => Self::Rgba16f,
            HdrEncodingSetting::AstcHdr => Self::AstcHdr,
            HdrEncodingSetting::Auto if platform == "android" => Self::Rgba16f,
            HdrEncodingSetting::Auto => Self::Bc6h,
        }
    }

    fn vk_format(self) -> ktx2::Format {
        match self {
            Self::Bc6h => ktx2::Format::BC6H_UFLOAT_BLOCK,
            Self::Rgba16f => ktx2::Format::R16G16B16A16_SFLOAT,
            Self::AstcHdr => ktx2::Format::ASTC_4x4_SFLOAT_BLOCK,
        }
    }

    /// Block width and height, texels, and bytes a block (a texel for RGBA16F).
    fn block(self) -> (u32, u32, usize) {
        match self {
            Self::Rgba16f => (1, 1, 8),
            Self::Bc6h | Self::AstcHdr => (4, 4, 16),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bc6h => "bc6h",
            Self::Rgba16f => "rgba16f",
            Self::AstcHdr => "astc_hdr",
        }
    }
}

/// Why an HDR source could not be cooked. The display text starts with the error's code.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HdrImportError {
    /// The file is not an image the decoder reads.
    #[error("hdr_decode: {0}")]
    Decode(String),
    /// The image is empty or larger than [`HDR_MAX_TEXELS`].
    #[error("hdr_size: {0}")]
    Size(String),
    /// A `.meta` setting is out of range.
    #[error("hdr_settings: {0}")]
    Settings(String),
    /// The KTX2 file could not be written.
    #[error("hdr_ktx2: {0}")]
    Ktx2(String),
}

/// A validated source image: RGBA `f32`, row-major, top row first.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrSource {
    /// Width, texels.
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// `width * height` texels, all finite, RGB in `0..=65504`, alpha in `0..=1`.
    pub texels: Vec<[f32; 4]>,
    /// NaN and infinite samples replaced by 0 (cook warning).
    pub non_finite: u64,
    /// Negative samples clamped to 0.
    pub negative: u64,
    /// Samples above [`HDR_MAX_VALUE`] clamped to it.
    pub too_bright: u64,
}

/// Decodes and validates an `.exr` or `.hdr` file (`ext` is the lowercase extension).
///
/// # Errors
///
/// [`HdrImportError::Decode`] for an unreadable file, [`HdrImportError::Size`] for an empty one
/// or over [`HDR_MAX_TEXELS`].
pub fn decode_hdr_source(bytes: &[u8], ext: &str) -> Result<HdrSource, HdrImportError> {
    let _span = tracing::info_span!("hdr_decode", bytes = bytes.len()).entered();
    let format = if ext == "exr" {
        image::ImageFormat::OpenExr
    } else {
        image::ImageFormat::Hdr
    };
    let (w, h) = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| HdrImportError::Decode(e.to_string()))?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > HDR_MAX_TEXELS {
        return Err(HdrImportError::Size(format!(
            "{w}x{h} texels (1 to {HDR_MAX_TEXELS} allowed)"
        )));
    }
    let img = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| HdrImportError::Decode(e.to_string()))?
        .into_rgba32f();
    let (width, height) = img.dimensions();
    let mut out = HdrSource {
        width,
        height,
        texels: Vec::with_capacity(width as usize * height as usize),
        non_finite: 0,
        negative: 0,
        too_bright: 0,
    };
    for px in img.pixels() {
        let mut t = [0.0f32; 4];
        for (k, v) in px.0.iter().enumerate() {
            let v = *v;
            t[k] = if k == 3 {
                if v.is_finite() { v.clamp(0.0, 1.0) } else { 1.0 }
            } else if !v.is_finite() {
                out.non_finite += 1;
                0.0
            } else if v < 0.0 {
                out.negative += 1;
                0.0
            } else if v > HDR_MAX_VALUE {
                out.too_bright += 1;
                HDR_MAX_VALUE
            } else {
                v
            };
        }
        out.texels.push(t);
    }
    Ok(out)
}

/// One mip level of an RGBA float image.
#[derive(Clone, Debug, PartialEq)]
struct Plane {
    width: u32,
    height: u32,
    texels: Vec<[f32; 4]>,
}

impl Plane {
    /// The next smaller level: a 2×2 box filter, an odd edge's last sample taking the extra row
    /// or column (the same windows the 8-bit chain uses).
    fn halve(&self) -> Plane {
        let (w, h) = ((self.width / 2).max(1), (self.height / 2).max(1));
        let mut texels = Vec::with_capacity(w as usize * h as usize);
        for y in 0..h {
            let (y0, y1) = crate::texture_settings::box_window(y, h, self.height);
            for x in 0..w {
                let (x0, x1) = crate::texture_settings::box_window(x, w, self.width);
                let mut sum = [0.0f32; 4];
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        let t = self.texels[(yy * self.width + xx) as usize];
                        for (acc, v) in sum.iter_mut().zip(t) {
                            *acc += v;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as f32;
                texels.push(sum.map(|s| s / n));
            }
        }
        Plane {
            width: w,
            height: h,
            texels,
        }
    }

    /// This level and every smaller one down to 1×1.
    fn chain(self) -> Vec<Plane> {
        let mut out = vec![self];
        while let Some(last) = out.last() {
            if last.width == 1 && last.height == 1 {
                break;
            }
            let next = last.halve();
            out.push(next);
        }
        out
    }
}

/// Box-filters `src` down a level at a time until its largest edge is at most `max`.
fn fit_max(mut plane: Plane, max: Option<u32>) -> Plane {
    let Some(max) = max.filter(|m| *m > 0) else {
        return plane;
    };
    while plane.width.max(plane.height) > max && (plane.width > 1 || plane.height > 1) {
        plane = plane.halve();
    }
    plane
}

/// Converts an equirect image (the renderer's layout: `regia_asset::hdr_texture`) to six
/// `face_px × face_px` cube faces, `+X, -X, +Y, -Y, +Z, -Z`. Each cube texel averages an `n × n`
/// grid of bilinear samples across its footprint, `n` from the ratio of the two sampling rates
/// (1 to 8), so a bright small sun is filtered instead of skipped.
#[must_use]
pub fn equirect_to_cube(source: &HdrImage, face_px: u32) -> Vec<HdrImage> {
    let _span = tracing::info_span!("hdr_equirect_to_cube", face_px).entered();
    // Equirect texels per radian horizontally over cube texels per radian (about 2/π · face_px).
    let ratio = (f64::from(source.width) / std::f64::consts::TAU)
        / (f64::from(face_px) / std::f64::consts::FRAC_PI_2).max(1e-9);
    let n = (ratio.ceil() as u32).clamp(1, 8);
    let inv = 1.0 / (n * n) as f32;
    (0..6)
        .map(|face| {
            let mut texels = Vec::with_capacity(face_px as usize * face_px as usize);
            for y in 0..face_px {
                for x in 0..face_px {
                    let mut sum = [0.0f32; 3];
                    for sy in 0..n {
                        for sx in 0..n {
                            let u = (x as f32 + (sx as f32 + 0.5) / n as f32) / face_px as f32;
                            let v = (y as f32 + (sy as f32 + 0.5) / n as f32) / face_px as f32;
                            let c = source.sample_equirect(cube_face_direction(face, u, v));
                            for (acc, v) in sum.iter_mut().zip(c) {
                                *acc += v;
                            }
                        }
                    }
                    texels.push(sum.map(|s| s * inv));
                }
            }
            HdrImage {
                width: face_px,
                height: face_px,
                texels,
            }
        })
        .collect()
}

fn rgb_plane(image: &HdrImage) -> Plane {
    Plane {
        width: image.width,
        height: image.height,
        texels: image.texels.iter().map(|t| [t[0], t[1], t[2], 1.0]).collect(),
    }
}

fn encode_level(encoding: HdrEncoding, plane: &Plane) -> Result<Vec<u8>, HdrImportError> {
    let _span = tracing::info_span!(
        "hdr_encode_level",
        encoding = encoding.name(),
        w = plane.width,
        h = plane.height
    )
    .entered();
    match encoding {
        HdrEncoding::Rgba16f => {
            let mut out = Vec::with_capacity(plane.texels.len() * 8);
            for t in &plane.texels {
                for c in t {
                    out.extend_from_slice(&f32_to_f16_bits(*c).to_le_bytes());
                }
            }
            Ok(out)
        }
        HdrEncoding::Bc6h => {
            let rgb: Vec<f32> = plane.texels.iter().flat_map(|t| [t[0], t[1], t[2]]).collect();
            let options = Bc6hOptions {
                metric: Bc6hMetric::LogDomain,
                modes: Bc6hModes::MODE_11
                    .union(Bc6hModes::MODE_12)
                    .union(Bc6hModes::MODE_13)
                    .union(Bc6hModes::MODE_14),
            };
            encode_image(plane.width, plane.height, &rgb, Bc6hFormat::Unsigned, &options)
                .map_err(|e| HdrImportError::Ktx2(e.to_string()))
        }
        HdrEncoding::AstcHdr => {
            let rgb: Vec<[f32; 3]> = plane.texels.iter().map(|t| [t[0], t[1], t[2]]).collect();
            Ok(encode_hdr_blocks(
                HdrBlockFormat::AstcHdr4x4,
                plane.width,
                plane.height,
                &rgb,
                None,
            ))
        }
    }
}

/// Writes `levels` (`levels[l][face]`, the bytes of face `face` of mip `l`) as an uncompressed
/// KTX2 file. Each level starts on a 16-byte boundary; a cube holds its six faces back to back
/// in a level.
fn write_ktx2(
    width: u32,
    height: u32,
    encoding: HdrEncoding,
    faces: u32,
    levels: &[Vec<Vec<u8>>],
) -> Result<Vec<u8>, HdrImportError> {
    use ktx2::dfd::Block;
    use ktx2::{Header, Index, LevelIndex};
    let err = |m: String| HdrImportError::Ktx2(m);
    let format = encoding.vk_format();
    let (bw, bh, bytes) = encoding.block();
    let (basic, type_size) = ktx2::dfd::Basic::from_format(format).map_err(|e| err(e.to_string()))?;
    let dfd_body = Block::Basic(basic).to_vec();
    let dfd_total = 4 + dfd_body.len();
    let level_count = levels.len();
    let dfd_offset = Header::LENGTH + level_count * LevelIndex::LENGTH;
    let data_start = (dfd_offset + dfd_total).next_multiple_of(16);
    let mut payload: Vec<u8> = Vec::new();
    let mut index = Vec::with_capacity(level_count);
    let (mut w, mut h) = (width, height);
    for (l, level) in levels.iter().enumerate() {
        let expected = (w.div_ceil(bw) * h.div_ceil(bh)) as usize * bytes;
        if level.len() != faces as usize || level.iter().any(|f| f.len() != expected) {
            return Err(err(format!("level {l}: faces of {expected} bytes expected")));
        }
        while !(data_start + payload.len()).is_multiple_of(16) {
            payload.push(0);
        }
        let offset = (data_start + payload.len()) as u64;
        let mut length = 0u64;
        for face in level {
            payload.extend_from_slice(face);
            length += face.len() as u64;
        }
        index.push(LevelIndex {
            byte_offset: offset,
            byte_length: length,
            uncompressed_byte_length: length,
        });
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    let header = Header {
        format: Some(format),
        type_size,
        pixel_width: width,
        pixel_height: height,
        pixel_depth: 0,
        layer_count: 0,
        face_count: faces,
        level_count: level_count as u32,
        supercompression_scheme: None,
        index: Index {
            dfd_byte_offset: dfd_offset as u32,
            dfd_byte_length: dfd_total as u32,
            kvd_byte_offset: 0,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };
    let mut out = Vec::with_capacity(data_start + payload.len());
    out.extend_from_slice(&header.as_bytes());
    for l in &index {
        out.extend_from_slice(&l.as_bytes());
    }
    out.extend_from_slice(&(dfd_total as u32).to_le_bytes());
    out.extend_from_slice(&dfd_body);
    out.resize(data_start, 0);
    out.extend_from_slice(&payload);
    ktx2::Reader::new(&out).map_err(|e| err(e.to_string()))?;
    Ok(out)
}

/// Builds the cooked KTX2 of a validated source under `settings` for `platform`.
///
/// # Errors
///
/// [`HdrImportError::Settings`] for a `cube_face_px` outside [`CUBE_FACE_PX_RANGE`],
/// [`HdrImportError::Ktx2`] when an encoder or the writer refuses.
pub fn cook_hdr_ktx2(
    source: &HdrSource,
    settings: &TextureImporterSettings,
    platform: &str,
) -> Result<(CookedTexture, HdrEncoding), HdrImportError> {
    let _span = tracing::info_span!("cook_hdr_ktx2", w = source.width, h = source.height).entered();
    let encoding = HdrEncoding::choose(settings, platform);
    let base = fit_max(
        Plane {
            width: source.width,
            height: source.height,
            texels: source.texels.clone(),
        },
        settings.max_size,
    );
    let (width, height, levels) = match settings.cube_face_px {
        Some(px) => {
            if !CUBE_FACE_PX_RANGE.contains(&px) {
                return Err(HdrImportError::Settings(format!(
                    "cube_face_px {px} is outside {}..={}",
                    CUBE_FACE_PX_RANGE.start(),
                    CUBE_FACE_PX_RANGE.end()
                )));
            }
            let equirect = HdrImage {
                width: base.width,
                height: base.height,
                texels: base.texels.iter().map(|t| [t[0], t[1], t[2]]).collect(),
            };
            let chains: Vec<Vec<Plane>> = equirect_to_cube(&equirect, px)
                .iter()
                .map(|f| rgb_plane(f).chain())
                .collect();
            let count = chains[0].len();
            let mut levels = Vec::with_capacity(count);
            for l in 0..count {
                let faces = chains
                    .iter()
                    .map(|chain| encode_level(encoding, &chain[l]))
                    .collect::<Result<Vec<_>, _>>()?;
                levels.push(faces);
            }
            (px, px, (levels, 6))
        }
        None => {
            let (w, h) = (base.width, base.height);
            let levels = base
                .chain()
                .iter()
                .map(|p| encode_level(encoding, p).map(|b| vec![b]))
                .collect::<Result<Vec<_>, _>>()?;
            (w, h, (levels, 1))
        }
    };
    let ktx2 = write_ktx2(width, height, encoding, levels.1, &levels.0)?;
    Ok((
        CookedTexture {
            width,
            height,
            usage: TextureCookUsage::ColorLinear,
            ktx2: ktx2.into(),
        },
        encoding,
    ))
}

/// The cook of one HDR source: what `cook.rs` records.
pub(crate) struct HdrCook {
    pub(crate) settings_hash: [u8; 32],
    pub(crate) id: regia_asset::AssetId,
    pub(crate) payload: Vec<u8>,
}

/// The type path of the cooked payload.
pub(crate) const HDR_TYPE_PATH: &str = "regia_asset::CookedTexture";

/// Cooks one `.exr` or `.hdr` source for `platform`. Replaced samples are queued as a cook
/// warning ([`crate::subasset_remap::take_warnings`]).
pub(crate) fn cook_hdr(
    source: &Path,
    bytes: &[u8],
    ext: &str,
    rel_path: &str,
    project_root: &Path,
    platform: &str,
) -> Result<HdrCook, CookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "cook_hdr", path = %source.display()).entered();
    let fail = |e: HdrImportError| CookError::Import(crate::ImportError::Image(e.to_string()));
    let meta = AssetDatabase::load_or_create_meta(source, ext).map_err(CookError::Db)?;
    let resolved =
        crate::import_rules::resolve_for_cook(project_root, rel_path, ext, meta.settings.as_deref());
    let settings =
        TextureImporterSettings::from_meta_string((!resolved.is_empty()).then_some(resolved.as_ref()));
    let decoded = decode_hdr_source(bytes, ext).map_err(fail)?;
    if decoded.non_finite > 0 {
        crate::subasset_remap::push_warnings([format!(
            "{}: {} NaN or infinite sample(s) replaced with 0",
            source.display(),
            decoded.non_finite
        )]);
    }
    if decoded.negative > 0 || decoded.too_bright > 0 {
        tracing::info!(
            target: "regia_asset_pipeline",
            "{}: {} negative sample(s) clamped to 0, {} above {HDR_MAX_VALUE} clamped",
            source.display(),
            decoded.negative,
            decoded.too_bright
        );
    }
    let (texture, encoding) = cook_hdr_ktx2(&decoded, &settings, platform).map_err(fail)?;
    let settings_hash =
        *blake3::hash(format!("{};hdr;encoding={}", settings.fingerprint(), encoding.name()).as_bytes())
            .as_bytes();
    Ok(HdrCook {
        settings_hash,
        id: meta.id,
        payload: texture.to_postcard().map_err(CookError::Postcard)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::hdr_texture::{decode_hdr_ktx2, equirect_direction};

    /// A smooth, strictly positive sky as a function of direction (linear in it, so a bilinear
    /// or block-compressed copy stays close).
    fn sky(dir: [f32; 3]) -> [f32; 3] {
        [
            2.0 + 1.5 * dir[1] + 0.3 * dir[0],
            3.0 + 2.0 * dir[1],
            5.0 + 3.0 * dir[1] - 0.5 * dir[2],
        ]
    }

    fn sky_texel(x: u32, y: u32, w: u32, h: u32) -> [f32; 3] {
        sky(equirect_direction(
            (x as f32 + 0.5) / w as f32,
            (y as f32 + 0.5) / h as f32,
        ))
    }

    fn exr_bytes(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 4]) -> Vec<u8> {
        let mut img = image::Rgba32FImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba(f(x, y));
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba32F(img)
            .write_to(&mut out, image::ImageFormat::OpenExr)
            .expect("write exr");
        out.into_inner()
    }

    fn hdr_bytes(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> Vec<u8> {
        let mut img = image::Rgb32FImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb(f(x, y));
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb32F(img)
            .write_to(&mut out, image::ImageFormat::Hdr)
            .expect("write hdr");
        out.into_inner()
    }

    fn sky_exr(w: u32, h: u32) -> Vec<u8> {
        exr_bytes(w, h, |x, y| {
            let c = sky_texel(x, y, w, h);
            [c[0], c[1], c[2], 1.0]
        })
    }

    /// Largest per-channel error of `got` against `want`, relative to the texel's brightest channel.
    fn max_rel_error(want: &[[f32; 3]], got: &[[f32; 3]]) -> f32 {
        assert_eq!(want.len(), got.len());
        want.iter()
            .zip(got)
            .map(|(w, g)| {
                let peak = w.iter().copied().fold(0.0f32, f32::max).max(1e-3);
                w.iter()
                    .zip(g)
                    .map(|(a, b)| (a - b).abs() / peak)
                    .fold(0.0, f32::max)
            })
            .fold(0.0, f32::max)
    }

    fn within(got: [f32; 3], want: [f32; 3], rel: f32) -> bool {
        got.iter().zip(want).all(|(g, w)| (g - w).abs() <= rel * w)
    }

    fn rgb(source: &HdrSource) -> Vec<[f32; 3]> {
        source.texels.iter().map(|t| [t[0], t[1], t[2]]).collect()
    }

    fn cook(source: &HdrSource, settings: &str, platform: &str) -> CookedTexture {
        let settings = TextureImporterSettings::from_meta_string(Some(settings));
        cook_hdr_ktx2(source, &settings, platform).expect("cook").0
    }

    #[test]
    fn an_exr_cooks_to_bc6h_with_a_full_chain_that_matches_the_source() {
        let source = decode_hdr_source(&sky_exr(64, 32), "exr").expect("decode");
        assert_eq!((source.width, source.height), (64, 32));
        assert_eq!((source.non_finite, source.negative, source.too_bright), (0, 0, 0));
        let tex = cook(&source, "", "linux");
        let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::BC6H_UFLOAT_BLOCK));
        assert_eq!(reader.levels().count(), 7, "64x32 down to 1x1");
        // The cook's own BC6H decoder (all modes) and the runtime's reader agree on the texels.
        let level0 = reader.levels().next().expect("level 0").data;
        let full = crate::bc6h_encode::decode_image(64, 32, level0, Bc6hFormat::Unsigned).expect("decode");
        let full: Vec<[f32; 3]> = full.as_chunks::<3>().0.to_vec();
        let runtime = decode_hdr_ktx2(&tex.ktx2).expect("runtime");
        assert_eq!(runtime.mip_count, 7);
        assert_eq!(runtime.faces.len(), 1);
        assert!(max_rel_error(&full, &runtime.faces[0].texels) < 1e-6);
        // The stated bound: 8 % of the texel's brightest channel (measured below it).
        let error = max_rel_error(&rgb(&source), &full);
        assert!(error <= HDR_BC6H_MAX_REL_ERROR, "BC6H error {error}");
    }

    #[test]
    fn a_radiance_hdr_cooks_the_same_way() {
        let (w, h) = (32, 16);
        let source =
            decode_hdr_source(&hdr_bytes(w, h, |x, y| sky_texel(x, y, w, h)), "hdr").expect("decode");
        assert_eq!((source.width, source.height), (w, h));
        // RGBE keeps about 1/128 relative precision.
        let want: Vec<[f32; 3]> = (0..h)
            .flat_map(|y| (0..w).map(move |x| sky_texel(x, y, w, h)))
            .collect();
        assert!(max_rel_error(&want, &rgb(&source)) < 0.02);
        let tex = cook(&source, "", "windows");
        let runtime = decode_hdr_ktx2(&tex.ktx2).expect("runtime");
        let error = max_rel_error(&rgb(&source), &runtime.faces[0].texels);
        assert!(error <= HDR_BC6H_MAX_REL_ERROR, "BC6H error {error}");
    }

    #[test]
    fn bad_samples_are_replaced_and_counted() {
        let bytes = exr_bytes(8, 8, |x, y| match (x, y) {
            (1, 1) => [f32::NAN, 1.0, 1.0, 1.0],
            (2, 1) => [f32::INFINITY, f32::NEG_INFINITY, 1.0, 1.0],
            (3, 1) => [-4.0, 1.0, 1.0, f32::NAN],
            (4, 1) => [1.0e9, 1.0, 1.0, 7.0],
            _ => [1.0, 1.0, 1.0, 1.0],
        });
        let s = decode_hdr_source(&bytes, "exr").expect("decode");
        assert_eq!(s.non_finite, 3, "a NaN and two infinities");
        assert_eq!(s.negative, 1);
        assert_eq!(s.too_bright, 1);
        assert!(
            s.texels
                .iter()
                .all(|t| t.iter().all(|c| c.is_finite() && *c >= 0.0))
        );
        assert!(s.texels.iter().all(|t| t[3] <= 1.0));
        assert_eq!(s.texels[8 + 1], [0.0, 1.0, 1.0, 1.0]);
        assert_eq!(s.texels[8 + 3][0], 0.0);
        assert_eq!(s.texels[8 + 4][0], HDR_MAX_VALUE);
    }

    #[test]
    fn rgba16f_android_and_astc_hdr_encodings_decode_back() {
        let source = decode_hdr_source(&sky_exr(32, 16), "exr").expect("decode");
        let want = rgb(&source);
        // Android defaults to RGBA16F: half precision.
        let tex = cook(&source, "", "android");
        let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::R16G16B16A16_SFLOAT));
        assert_eq!(reader.levels().count(), 6);
        let got = decode_hdr_ktx2(&tex.ktx2).expect("decode").faces.remove(0);
        assert!(max_rel_error(&want, &got.texels) < 1.0e-3);
        // `compress: Some(false)` is RGBA16F on desktop too.
        let tex = cook(&source, "(compress: Some(false))", "linux");
        let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::R16G16B16A16_SFLOAT));
        // ASTC HDR on request.
        let tex = cook(&source, "(hdr_encoding: Some(astc_hdr))", "linux");
        let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
        assert_eq!(reader.header().format, Some(ktx2::Format::ASTC_4x4_SFLOAT_BLOCK));
        let got = decode_hdr_ktx2(&tex.ktx2).expect("decode").faces.remove(0);
        let error = max_rel_error(&want, &got.texels);
        assert!(error <= HDR_BC6H_MAX_REL_ERROR, "ASTC HDR error {error}");
    }

    #[test]
    fn max_size_halves_the_source_before_the_chain() {
        let source = decode_hdr_source(&sky_exr(64, 32), "exr").expect("decode");
        let tex = cook(&source, "(max_size: Some(16))", "linux");
        assert_eq!((tex.width, tex.height), (16, 8));
        assert_eq!(decode_hdr_ktx2(&tex.ktx2).expect("decode").mip_count, 5);
    }

    #[test]
    fn cube_faces_sample_the_equirect_in_every_direction_and_across_seams() {
        let (w, h) = (128u32, 64u32);
        let source = decode_hdr_source(&sky_exr(w, h), "exr").expect("decode");
        let tex = cook(
            &source,
            "(cube_face_px: Some(16), compress: Some(false))",
            "linux",
        );
        assert_eq!((tex.width, tex.height), (16, 16));
        let cube = decode_hdr_ktx2(&tex.ktx2).expect("decode");
        assert!(cube.is_cube());
        assert_eq!(cube.mip_count, 5, "16 down to 1");
        // The six axis directions: the four centre texels of a face meet at its axis.
        let axes = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
        ];
        for (face, axis) in axes.iter().enumerate() {
            let img = &cube.faces[face];
            let mut mean = [0.0f32; 3];
            for (x, y) in [(7, 7), (8, 7), (7, 8), (8, 8)] {
                let t = img.texels[y * 16 + x];
                for (m, v) in mean.iter_mut().zip(t) {
                    *m += v / 4.0;
                }
            }
            let want = sky(*axis);
            assert!(
                within(mean, want, 0.03),
                "face {face} axis {axis:?}: {mean:?} vs {want:?}"
            );
        }
        // Every texel of every face, which includes the faces' shared edges, the equirect's
        // azimuth seam (the -X face) and both poles.
        for (face, img) in cube.faces.iter().enumerate() {
            for y in 0..16u32 {
                for x in 0..16u32 {
                    let dir = cube_face_direction(face, (x as f32 + 0.5) / 16.0, (y as f32 + 0.5) / 16.0);
                    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
                    let want = sky(dir.map(|c| c / len));
                    let got = img.texels[(y * 16 + x) as usize];
                    assert!(
                        within(got, want, 0.04),
                        "face {face} texel ({x},{y}): {got:?} vs {want:?}"
                    );
                }
            }
        }
        // Neighbouring texels either side of a shared edge differ by a smooth step only.
        let step = |a: [f32; 3], b: [f32; 3]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).abs() / x)
                .fold(0.0, f32::max)
        };
        // +X face (0) right edge meets -Z face (5) left edge.
        for y in 0..16usize {
            assert!(
                step(cube.faces[0].texels[y * 16 + 15], cube.faces[5].texels[y * 16]) < 0.08,
                "row {y}"
            );
        }
        // -X face (1) right edge meets +Z face (4) left edge, across the equirect's azimuth seam.
        for y in 0..16usize {
            assert!(
                step(cube.faces[1].texels[y * 16 + 15], cube.faces[4].texels[y * 16]) < 0.08,
                "row {y}"
            );
        }
    }

    #[test]
    fn a_cube_face_size_outside_the_range_is_refused() {
        let source = decode_hdr_source(&sky_exr(16, 8), "exr").expect("decode");
        for px in [0u32, 2, 8193] {
            let settings = TextureImporterSettings {
                cube_face_px: Some(px),
                ..TextureImporterSettings::default()
            };
            let err = cook_hdr_ktx2(&source, &settings, "linux").expect_err("range");
            assert!(err.to_string().starts_with("hdr_settings:"), "{err}");
        }
    }

    #[test]
    fn a_damaged_file_is_refused_with_a_code() {
        let err = decode_hdr_source(b"not an image", "exr").expect_err("garbage");
        assert!(err.to_string().starts_with("hdr_decode:"), "{err}");
        let err = decode_hdr_source(&[], "hdr").expect_err("empty");
        assert!(err.to_string().starts_with("hdr_decode:"), "{err}");
    }
}
