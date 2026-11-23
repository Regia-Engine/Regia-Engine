// SPDX-License-Identifier: MPL-2.0
//! `FloatImage`, the single-channel R32F image the texture helpers work on, and its conversions
//! to the engine's image forms: RGBA8 ([`CookedImage`]) and R16 ([`regia_asset::Heightfield`]
//! samples).

use regia_asset::CookedImage;

use crate::GenError;

/// The most pixels one image may have (64 Mi: 8192 × 8192).
pub const MAX_IMAGE_PIXELS: usize = 1 << 26;

/// A single-channel 32-bit float image, row-major, top row first. Values are usually in `[0, 1]`
/// (masks, noise); the conversions clamp to that range.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` values.
    pub pixels: Vec<f32>,
}

/// Minimum, maximum and mean of an image or a heightfield.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageStats {
    /// Smallest value.
    pub min: f32,
    /// Largest value.
    pub max: f32,
    /// Mean, summed in row-major order in `f64`.
    pub mean: f64,
}

pub(crate) fn check_size(width: u32, height: u32) -> Result<usize, GenError> {
    if width == 0 || height == 0 {
        return Err(GenError::InvalidInput(format!(
            "image size {width}x{height} has a zero side"
        )));
    }
    let n = width as usize * height as usize;
    if n > MAX_IMAGE_PIXELS {
        return Err(GenError::TooLarge {
            what: "pixel count",
            value: n,
            limit: MAX_IMAGE_PIXELS,
        });
    }
    Ok(n)
}

pub(crate) fn stats_of(values: &[f32]) -> ImageStats {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    for &v in values {
        min = min.min(v);
        max = max.max(v);
        sum += f64::from(v);
    }
    let mean = if values.is_empty() {
        0.0
    } else {
        sum / values.len() as f64
    };
    ImageStats { min, max, mean }
}

/// `v` clamped to `[0, 1]` and rounded to 8 bits (`NaN` gives 0).
pub fn quantize_u8(v: f32) -> u8 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    (v * 255.0 + 0.5).floor() as u8
}

/// `v` clamped to `[0, 1]` and rounded to 16 bits (`NaN` gives 0).
pub fn quantize_u16(v: f32) -> u16 {
    let v = if v.is_nan() {
        0.0
    } else {
        f64::from(v.clamp(0.0, 1.0))
    };
    (v * 65535.0 + 0.5).floor() as u16
}

impl FloatImage {
    /// An image filled with `value`.
    pub fn new(width: u32, height: u32, value: f32) -> Result<Self, GenError> {
        let n = check_size(width, height)?;
        Ok(Self {
            width,
            height,
            pixels: vec![value; n],
        })
    }

    /// An image whose pixel `(x, y)` is `f(x, y)`, filled in row-major order.
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> f32) -> Result<Self, GenError> {
        let n = check_size(width, height)?;
        let mut pixels = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                pixels.push(f(x, y));
            }
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// The pixel at `(x, y)`, or `None` outside the image.
    pub fn get(&self, x: u32, y: u32) -> Option<f32> {
        (x < self.width && y < self.height)
            .then(|| {
                self.pixels
                    .get(y as usize * self.width as usize + x as usize)
                    .copied()
            })
            .flatten()
    }

    /// Minimum, maximum and mean.
    pub fn stats(&self) -> ImageStats {
        stats_of(&self.pixels)
    }

    /// The image as gray RGBA8 (R = G = B = the value, A = 255), each value clamped to `[0, 1]`
    /// and rounded ([`quantize_u8`]). The bytes are linear values: a mask, not an sRGB color.
    pub fn to_rgba8(&self) -> CookedImage {
        let mut rgba8 = Vec::with_capacity(self.pixels.len() * 4);
        for &v in &self.pixels {
            let q = quantize_u8(v);
            rgba8.extend_from_slice(&[q, q, q, 255]);
        }
        CookedImage {
            width: self.width,
            height: self.height,
            rgba8,
        }
    }

    /// The image as R16 samples ([`quantize_u16`]) in the engine's 16-bit form
    /// ([`regia_asset::Heightfield`], row-major, top row first).
    pub fn to_r16(&self) -> regia_asset::Heightfield {
        regia_asset::Heightfield {
            width: self.width,
            height: self.height,
            samples: self.pixels.iter().map(|&v| quantize_u16(v)).collect(),
        }
    }

    /// The R32F bytes: each value's bits, little endian, row-major.
    pub fn r32f_bytes(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .flat_map(|v| v.to_bits().to_le_bytes())
            .collect()
    }

    pub(crate) fn same_size(&self, other: &Self) -> Result<(), GenError> {
        if (self.width, self.height) == (other.width, other.height) {
            Ok(())
        } else {
            Err(GenError::InvalidInput(format!(
                "image sizes differ: {}x{} and {}x{}",
                self.width, self.height, other.width, other.height
            )))
        }
    }
}

/// Packs four same-sized images into one RGBA8 image (R, G, B, A in that order, each quantized
/// with [`quantize_u8`]): for example a wear, grime, edge and cavity mask in one texture.
pub fn pack_rgba8(channels: [&FloatImage; 4]) -> Result<CookedImage, GenError> {
    let [r, g, b, a] = channels;
    for c in [g, b, a] {
        r.same_size(c)?;
    }
    let mut rgba8 = Vec::with_capacity(r.pixels.len() * 4);
    for i in 0..r.pixels.len() {
        rgba8.extend_from_slice(&[
            quantize_u8(r.pixels[i]),
            quantize_u8(g.pixels[i]),
            quantize_u8(b.pixels[i]),
            quantize_u8(a.pixels[i]),
        ]);
    }
    Ok(CookedImage {
        width: r.width,
        height: r.height,
        rgba8,
    })
}

fn hasher(domain: &[u8], width: u32, height: u32) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(domain);
    h.update(&width.to_le_bytes());
    h.update(&height.to_le_bytes());
    h
}

/// A BLAKE3 hash (hex) of a [`FloatImage`]'s size and value bits; tests pin it across platforms.
pub fn image_hash(image: &FloatImage) -> String {
    let mut h = hasher(b"regia_gen.image.r32f.v1", image.width, image.height);
    for v in &image.pixels {
        h.update(&v.to_bits().to_le_bytes());
    }
    h.finalize().to_hex().to_string()
}

/// A BLAKE3 hash (hex) of an RGBA8 image's size and bytes.
pub fn rgba8_hash(image: &CookedImage) -> String {
    let mut h = hasher(b"regia_gen.image.rgba8.v1", image.width, image.height);
    h.update(&image.rgba8);
    h.finalize().to_hex().to_string()
}

/// A BLAKE3 hash (hex) of a 16-bit heightfield's size and samples (little endian).
pub fn r16_hash(field: &regia_asset::Heightfield) -> String {
    let mut h = hasher(b"regia_gen.image.r16.v1", field.width, field.height);
    for s in &field.samples {
        h.update(&s.to_le_bytes());
    }
    h.finalize().to_hex().to_string()
}
