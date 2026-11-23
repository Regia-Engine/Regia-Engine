// SPDX-License-Identifier: MPL-2.0
//! CPU reference: YUV to RGB and frame hashing (specs/50 § 3.1, § 7). The GPU convert pass must match
//! this within ±1 per channel. Matrices and ranges follow ITU-R BT.601-7, BT.709-6 and BT.2020-2 with
//! ITU-T H.273 code points (`docs/provenance.md`); chroma is upsampled by nearest sample (co-sited left).

use serde::{Deserialize, Serialize};

use crate::codec::DecodedPicture;

/// A YUV to RGB matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matrix {
    /// ITU-R BT.601 (H.273 codes 5 and 6).
    Bt601,
    /// ITU-R BT.709 (code 1, and the default for unspecified).
    Bt709,
    /// ITU-R BT.2020 non-constant luminance (code 9).
    Bt2020Ncl,
}

impl Matrix {
    /// The matrix for an H.273 `matrix_coefficients` code; unknown codes use BT.709.
    pub fn from_h273(code: u8) -> Self {
        match code {
            5 | 6 => Self::Bt601,
            9 => Self::Bt2020Ncl,
            _ => Self::Bt709,
        }
    }

    /// Luma weights `(Kr, Kb)`.
    pub fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020Ncl => (0.2627, 0.0593),
        }
    }
}

/// Converts one YUV sample triple of `bit_depth` bits to 8-bit R'G'B' (the encoded values a sRGB view
/// displays). Rounded to nearest, clamped.
pub fn yuv_to_rgb8(y: u16, u: u16, v: u16, bit_depth: u8, matrix: Matrix, full_range: bool) -> [u8; 3] {
    let depth = i32::from(bit_depth.clamp(8, 16));
    let scale = f64::from(1u32 << (depth - 8));
    let (yn, cb, cr) = if full_range {
        let max = f64::from((1u32 << depth) - 1);
        let mid = f64::from(1u32 << (depth - 1));
        (
            f64::from(y) / max,
            (f64::from(u) - mid) / max,
            (f64::from(v) - mid) / max,
        )
    } else {
        (
            (f64::from(y) - 16.0 * scale) / (219.0 * scale),
            (f64::from(u) - 128.0 * scale) / (224.0 * scale),
            (f64::from(v) - 128.0 * scale) / (224.0 * scale),
        )
    };
    let (kr, kb) = matrix.kr_kb();
    let r = yn + 2.0 * (1.0 - kr) * cr;
    let b = yn + 2.0 * (1.0 - kb) * cb;
    let g = (yn - kr * r - kb * b) / (1.0 - kr - kb);
    let q = |c: f64| (c * 255.0).round().clamp(0.0, 255.0) as u8;
    [q(r), q(g), q(b)]
}

/// Converts a whole picture to tightly packed RGBA8 (alpha 255) using its matrix and range.
/// `color.transfer` is not read: HDR (PQ, HLG) is not tone-mapped.
pub fn picture_to_rgba8(p: &DecodedPicture) -> Vec<u8> {
    let matrix = Matrix::from_h273(p.color.matrix);
    let (sx, sy) = p.chroma.shifts();
    let mut out = Vec::with_capacity(p.width as usize * p.height as usize * 4);
    for y in 0..p.height {
        for x in 0..p.width {
            let rgb = yuv_to_rgb8(
                p.sample(0, x, y),
                p.sample(1, x >> sx, y >> sy),
                p.sample(2, x >> sx, y >> sy),
                p.bit_depth,
                matrix,
                p.color.full_range,
            );
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    out
}

/// FNV-1a 64-bit hash (the `video.frame_hash` hash).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A rectangle region of an RGBA8 image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// FNV-1a of an RGBA8 image (`width` x `height`), optionally of one region (clamped to the image).
pub fn rgba_hash(rgba: &[u8], width: u32, height: u32, region: Option<Region>) -> u64 {
    let Some(r) = region else {
        return fnv1a64(rgba);
    };
    let x0 = r.x.min(width);
    let y0 = r.y.min(height);
    let x1 = r.x.saturating_add(r.w).min(width);
    let y1 = r.y.saturating_add(r.h).min(height);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for y in y0..y1 {
        let start = (y as usize * width as usize + x0 as usize) * 4;
        let end = (y as usize * width as usize + x1 as usize) * 4;
        for b in rgba.get(start..end).unwrap_or(&[]) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn limited_range_black_white_and_primaries() {
        for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020Ncl] {
            assert_eq!(yuv_to_rgb8(16, 128, 128, 8, m, false), [0, 0, 0]);
            assert_eq!(yuv_to_rgb8(235, 128, 128, 8, m, false), [255, 255, 255]);
            assert_eq!(yuv_to_rgb8(64, 512, 512, 10, m, false), [0, 0, 0]);
            assert_eq!(yuv_to_rgb8(940, 512, 512, 10, m, false), [255, 255, 255]);
            assert_eq!(yuv_to_rgb8(0, 128, 128, 8, m, true), [0, 0, 0]);
            assert_eq!(yuv_to_rgb8(255, 128, 128, 8, m, true), [255, 255, 255]);
        }
    }

    /// Ramp test (specs/50 § 8 test 10): encode known RGB with the forward matrix, decode, compare ±1.
    #[test]
    fn ramp_round_trips_within_one() {
        for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020Ncl] {
            let (kr, kb) = m.kr_kb();
            for full in [false, true] {
                for i in 0..=32u32 {
                    let rgb = [
                        (i * 7 % 256) as f64,
                        (i * 8).min(255) as f64,
                        (255 - i * 5) as f64,
                    ];
                    let (r, g, b) = (rgb[0] / 255.0, rgb[1] / 255.0, rgb[2] / 255.0);
                    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
                    let cb = (b - y) / (2.0 * (1.0 - kb));
                    let cr = (r - y) / (2.0 * (1.0 - kr));
                    let enc = |v: f64, lo: f64, span: f64| (lo + v * span).round().clamp(0.0, 1023.0) as u16;
                    let (yy, uu, vv) = if full {
                        (
                            enc(y, 0.0, 1023.0),
                            enc(cb, 512.0, 1023.0),
                            enc(cr, 512.0, 1023.0),
                        )
                    } else {
                        (enc(y, 64.0, 876.0), enc(cb, 512.0, 896.0), enc(cr, 512.0, 896.0))
                    };
                    let out = yuv_to_rgb8(yy, uu, vv, 10, m, full);
                    for c in 0..3 {
                        let d = (f64::from(out[c]) - rgb[c]).abs();
                        assert!(d <= 1.0, "{m:?} full={full} i={i} c={c}: {out:?} vs {rgb:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn region_hash_matches_cropped_copy() {
        let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        let mut crop = Vec::new();
        for y in 1..3 {
            crop.extend_from_slice(&rgba[(y * 4 + 1) * 4..(y * 4 + 3) * 4]);
        }
        let r = Region {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        };
        assert_eq!(rgba_hash(&rgba, 4, 4, Some(r)), fnv1a64(&crop));
        assert_eq!(rgba_hash(&rgba, 4, 4, None), fnv1a64(&rgba));
    }

    /// Spec 50 § 2.7: BT.709 is the default when the matrix is absent (H.273 code 2, "unspecified")
    /// or unknown; BT.601 (codes 5, 6) and BT.2020 non-constant luminance (code 9) are honored.
    #[test]
    fn matrix_codes_default_to_bt709() {
        for code in [0, 1, 2, 3, 4, 7, 8, 10, 255] {
            assert_eq!(Matrix::from_h273(code), Matrix::Bt709, "code {code}");
        }
        assert_eq!(Matrix::from_h273(5), Matrix::Bt601);
        assert_eq!(Matrix::from_h273(6), Matrix::Bt601);
        assert_eq!(Matrix::from_h273(9), Matrix::Bt2020Ncl);
    }
}
