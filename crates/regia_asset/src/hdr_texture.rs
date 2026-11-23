// SPDX-License-Identifier: MPL-2.0
//! Reading cooked HDR textures on the CPU (ADR-2546): a cooked `.exr` / `.hdr` is a KTX2 holding
//! BC6H (desktop), RGBA16F or ASTC 4×4 HDR texels, as one 2D equirect image or a six-face cube.
//! [`decode_hdr_ktx2`] turns level 0 of such a file into linear `f32` RGB, so the studio
//! environment can be set from a cooked asset ([`cube_to_equirect`] resamples a cube to the
//! equirect layout the renderer's environment maps use). The direction conventions
//! ([`equirect_direction`], [`cube_face_direction`]) are shared with the cook's equirect-to-cube
//! conversion, so both sides agree on which texel looks where.
//!
//! Conventions, written from the public Vulkan specification ("Cube Map Face Selection") and the
//! renderer's own equirect layout: a cube is stored `+X, -X, +Y, -Y, +Z, -Z`, rows top to
//! bottom; an equirect column is azimuth `atan2(z, x)` (the centre looks along +X) and its top
//! row looks straight up (+Y).

use thiserror::Error;

use crate::hdr_blocks::{HdrBlockFormat, decode_hdr_blocks, f16_bits_to_f32};

/// The largest edge, texels, a decoded HDR texture may have (a hostile file cannot ask for more).
pub const HDR_TEXTURE_MAX_EDGE: u32 = 16_384;

/// Why a cooked HDR texture could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HdrTextureError {
    /// The bytes are not a KTX2 file the reader accepts.
    #[error("hdr_texture_ktx2: {0}")]
    Ktx2(String),
    /// The file's format is not an HDR format this reader decodes.
    #[error("hdr_texture_format: {0} is not BC6H, RGBA16F or ASTC 4x4 HDR")]
    Format(String),
    /// A face count other than 1 (2D) or 6 (cube), an array, a volume or a size over the limit.
    #[error("hdr_texture_shape: {0}")]
    Shape(String),
    /// Level 0 holds fewer bytes than its size needs, or a block the decoder does not read.
    #[error("hdr_texture_data: {0}")]
    Data(String),
}

impl HdrTextureError {
    /// The machine-readable code (`hdr_texture_ktx2`, `hdr_texture_format`, `hdr_texture_shape`,
    /// `hdr_texture_data`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ktx2(_) => "hdr_texture_ktx2",
            Self::Format(_) => "hdr_texture_format",
            Self::Shape(_) => "hdr_texture_shape",
            Self::Data(_) => "hdr_texture_data",
        }
    }
}

/// A linear RGB float image, row-major, top row first.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrImage {
    /// Width, texels.
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// `width * height` texels.
    pub texels: Vec<[f32; 3]>,
}

/// Level 0 of a cooked HDR texture, decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrTextureLevel {
    /// One image for a 2D texture, six (`+X, -X, +Y, -Y, +Z, -Z`) for a cube.
    pub faces: Vec<HdrImage>,
    /// Number of mip levels the file holds.
    pub mip_count: u32,
}

impl HdrTextureLevel {
    /// Whether the texture is a six-face cube.
    #[must_use]
    pub fn is_cube(&self) -> bool {
        self.faces.len() == 6
    }
}

/// The unit direction an equirect map looks toward at `(u, v)` in `[0, 1]²` (`v` = 0 is up).
#[must_use]
pub fn equirect_direction(u: f32, v: f32) -> [f32; 3] {
    let phi = (u - 0.5) * std::f32::consts::TAU;
    let theta = v * std::f32::consts::PI;
    let (st, ct) = theta.sin_cos();
    let (sp, cp) = phi.sin_cos();
    [st * cp, ct, st * sp]
}

/// The `(u, v)` an equirect map stores direction `dir` at (the inverse of
/// [`equirect_direction`]; `dir` need not be normalized).
#[must_use]
pub fn direction_to_equirect(dir: [f32; 3]) -> (f32, f32) {
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2])
        .sqrt()
        .max(1e-20);
    let u = dir[2].atan2(dir[0]) / std::f32::consts::TAU + 0.5;
    let v = (dir[1] / len).clamp(-1.0, 1.0).acos() / std::f32::consts::PI;
    (u, v)
}

/// The (unnormalized) direction cube face `face` (0..6: `+X, -X, +Y, -Y, +Z, -Z`) looks toward at
/// `(u, v)` in `[0, 1]²` (`v` = 0 is the face's top row).
#[must_use]
pub fn cube_face_direction(face: usize, u: f32, v: f32) -> [f32; 3] {
    let sc = 2.0 * u - 1.0;
    let tc = 2.0 * v - 1.0;
    match face {
        0 => [1.0, -tc, -sc],
        1 => [-1.0, -tc, sc],
        2 => [sc, 1.0, tc],
        3 => [sc, -1.0, -tc],
        4 => [sc, -tc, 1.0],
        _ => [-sc, -tc, -1.0],
    }
}

/// The cube face and `(u, v)` direction `dir` falls at (the inverse of [`cube_face_direction`]).
#[must_use]
pub fn direction_to_cube(dir: [f32; 3]) -> (usize, f32, f32) {
    let [x, y, z] = dir;
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if x >= 0.0 { (0, -z, -y, ax) } else { (1, z, -y, ax) }
    } else if ay >= az {
        if y >= 0.0 { (2, x, z, ay) } else { (3, x, -z, ay) }
    } else if z >= 0.0 {
        (4, x, -y, az)
    } else {
        (5, -x, -y, az)
    };
    let ma = ma.max(1e-20);
    (face, (sc / ma + 1.0) * 0.5, (tc / ma + 1.0) * 0.5)
}

impl HdrImage {
    fn texel(&self, x: i64, y: i64, wrap_x: bool) -> [f32; 3] {
        let (w, h) = (i64::from(self.width), i64::from(self.height));
        let x = if wrap_x {
            x.rem_euclid(w)
        } else {
            x.clamp(0, w - 1)
        };
        let y = y.clamp(0, h - 1);
        self.texels[(y * w + x) as usize]
    }

    /// Bilinear sample at `(u, v)` in `[0, 1]²`; `wrap_x` wraps the horizontal edge (an equirect's
    /// azimuth seam), otherwise edges clamp.
    #[must_use]
    pub fn sample_bilinear(&self, u: f32, v: f32, wrap_x: bool) -> [f32; 3] {
        let x = u * self.width as f32 - 0.5;
        let y = v * self.height as f32 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let a = self.texel(x0, y0, wrap_x);
        let b = self.texel(x0 + 1, y0, wrap_x);
        let c = self.texel(x0, y0 + 1, wrap_x);
        let d = self.texel(x0 + 1, y0 + 1, wrap_x);
        [0, 1, 2].map(|k| {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            top + (bottom - top) * fy
        })
    }

    /// Bilinear radiance of an equirect image toward `dir`.
    #[must_use]
    pub fn sample_equirect(&self, dir: [f32; 3]) -> [f32; 3] {
        let (u, v) = direction_to_equirect(dir);
        self.sample_bilinear(u, v, true)
    }
}

/// Resamples a six-face cube to a `width × height` equirect image (bilinear within the face the
/// direction falls on).
///
/// # Errors
///
/// [`HdrTextureError::Shape`] when `faces` is not six same-size square images or the target has
/// a zero edge.
pub fn cube_to_equirect(faces: &[HdrImage], width: u32, height: u32) -> Result<HdrImage, HdrTextureError> {
    let _span = tracing::info_span!("hdr_cube_to_equirect", width, height).entered();
    let ok = faces.len() == 6
        && faces
            .iter()
            .all(|f| f.width == faces[0].width && f.height == f.width && f.width > 0);
    if !ok || width == 0 || height == 0 {
        return Err(HdrTextureError::Shape(
            "a cube needs six equal square faces and a non-empty target".into(),
        ));
    }
    let mut texels = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            let dir = equirect_direction((x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32);
            let (face, u, v) = direction_to_cube(dir);
            texels.push(faces[face].sample_bilinear(u, v, false));
        }
    }
    Ok(HdrImage {
        width,
        height,
        texels,
    })
}

/// Decodes level 0 of a cooked HDR KTX2 (BC6H unsigned float, RGBA16F or ASTC 4×4 HDR; a 2D
/// texture or a cube) to linear `f32` RGB. Alpha is dropped. Block formats are read with the
/// one-region subset the cook writes.
///
/// # Errors
///
/// [`HdrTextureError`]: the file is not valid KTX2, is another format or shape, or is short.
pub fn decode_hdr_ktx2(ktx2_bytes: &[u8]) -> Result<HdrTextureLevel, HdrTextureError> {
    let _span = tracing::info_span!("decode_hdr_ktx2").entered();
    // The parser overflows on one hostile descriptor field; refuse it before it runs.
    if let Some(reason) = crate::cooked::hostile_ktx2_dfd(ktx2_bytes) {
        return Err(HdrTextureError::Ktx2(reason.to_string()));
    }
    let reader = ktx2::Reader::new(ktx2_bytes).map_err(|e| HdrTextureError::Ktx2(e.to_string()))?;
    let header = reader.header();
    if header.supercompression_scheme.is_some() {
        return Err(HdrTextureError::Ktx2(
            "supercompressed HDR textures are not read".into(),
        ));
    }
    let (width, height) = (header.pixel_width, header.pixel_height);
    if width == 0
        || height == 0
        || width > HDR_TEXTURE_MAX_EDGE
        || height > HDR_TEXTURE_MAX_EDGE
        || header.pixel_depth > 1
        || header.layer_count > 1
        || !(header.face_count == 1 || header.face_count == 6)
    {
        return Err(HdrTextureError::Shape(format!(
            "{width}x{height}, depth {}, {} layers, {} faces",
            header.pixel_depth, header.layer_count, header.face_count
        )));
    }
    let format = header
        .format
        .ok_or_else(|| HdrTextureError::Format("an undefined format".into()))?;
    let kind = if format == ktx2::Format::BC6H_UFLOAT_BLOCK {
        Kind::Block(HdrBlockFormat::Bc6h)
    } else if format == ktx2::Format::ASTC_4x4_SFLOAT_BLOCK {
        Kind::Block(HdrBlockFormat::AstcHdr4x4)
    } else if format == ktx2::Format::R16G16B16A16_SFLOAT {
        Kind::Half
    } else {
        return Err(HdrTextureError::Format(format!("{format:?}")));
    };
    let mip_count = reader.levels().len().max(1) as u32;
    let level0 = reader
        .levels()
        .next()
        .ok_or_else(|| HdrTextureError::Data("no level".into()))?
        .data;
    let faces = header.face_count as usize;
    let per_face = match kind {
        Kind::Half => width as usize * height as usize * 8,
        Kind::Block(_) => crate::hdr_blocks::hdr_block_bytes(width, height),
    };
    if level0.len() != per_face * faces {
        return Err(HdrTextureError::Data(format!(
            "level 0 holds {} bytes, {faces} face(s) of {width}x{height} need {}",
            level0.len(),
            per_face * faces
        )));
    }
    let mut out = Vec::with_capacity(faces);
    for data in level0.chunks_exact(per_face) {
        let texels = match kind {
            Kind::Half => data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|t| [0, 2, 4].map(|o| f16_bits_to_f32(u16::from_le_bytes([t[o], t[o + 1]]))))
                .collect(),
            Kind::Block(block) => decode_hdr_blocks(block, width, height, data)
                .map_err(|e| HdrTextureError::Data(e.to_string()))?,
        };
        out.push(HdrImage {
            width,
            height,
            texels,
        });
    }
    Ok(HdrTextureLevel {
        faces: out,
        mip_count,
    })
}

#[derive(Clone, Copy)]
enum Kind {
    Half,
    Block(HdrBlockFormat),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_round_trip() {
        for face in 0..6 {
            for (u, v) in [(0.5, 0.5), (0.1, 0.9), (0.97, 0.03)] {
                let d = cube_face_direction(face, u, v);
                let (f2, u2, v2) = direction_to_cube(d);
                assert_eq!(f2, face);
                assert!((u - u2).abs() < 1e-5 && (v - v2).abs() < 1e-5, "{face} {u} {v}");
            }
        }
        for (u, v) in [(0.5, 0.5), (0.2, 0.3), (0.9, 0.8)] {
            let (u2, v2) = direction_to_equirect(equirect_direction(u, v));
            assert!((u - u2).abs() < 1e-4 && (v - v2).abs() < 1e-4);
        }
    }

    #[test]
    fn face_centres_look_along_the_axes() {
        let axes = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
        ];
        for (face, axis) in axes.iter().enumerate() {
            assert_eq!(cube_face_direction(face, 0.5, 0.5), *axis);
        }
        // The equirect centre looks along +X, the top row up.
        let c = equirect_direction(0.5, 0.5);
        assert!((c[0] - 1.0).abs() < 1e-6 && c[1].abs() < 1e-6 && c[2].abs() < 1e-6);
        assert!(equirect_direction(0.3, 0.0)[1] > 0.999);
    }

    #[test]
    fn a_cube_resamples_to_the_equirect_it_came_from() {
        let f = |d: [f32; 3]| {
            let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            [2.0 + d[0] / len, 3.0 + d[1] / len, 4.0 + d[2] / len]
        };
        let n = 32u32;
        let faces: Vec<HdrImage> = (0..6)
            .map(|face| HdrImage {
                width: n,
                height: n,
                texels: (0..n * n)
                    .map(|i| {
                        let (x, y) = (i % n, i / n);
                        f(cube_face_direction(
                            face,
                            (x as f32 + 0.5) / n as f32,
                            (y as f32 + 0.5) / n as f32,
                        ))
                    })
                    .collect(),
            })
            .collect();
        let eq = cube_to_equirect(&faces, 64, 32).expect("resample");
        assert_eq!((eq.width, eq.height), (64, 32));
        for y in 0..32u32 {
            for x in 0..64u32 {
                let want = f(equirect_direction(
                    (x as f32 + 0.5) / 64.0,
                    (y as f32 + 0.5) / 32.0,
                ));
                let got = eq.texels[(y * 64 + x) as usize];
                assert!(
                    got.iter().zip(want).all(|(g, w)| (g - w).abs() < 0.03 * w),
                    "({x},{y}): {got:?} vs {want:?}"
                );
            }
        }
        assert!(cube_to_equirect(&faces[..5], 8, 4).is_err());
    }

    #[test]
    fn garbage_is_refused_with_a_code() {
        let err = decode_hdr_ktx2(b"not a ktx2 file").expect_err("garbage");
        assert_eq!(err.code(), "hdr_texture_ktx2");
    }
}
