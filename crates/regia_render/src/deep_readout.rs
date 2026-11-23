// SPDX-License-Identifier: MPL-2.0
//! Deep offscreen readout: the final (tonemapped, display-encoded) frame kept at 16 bits per channel
//! instead of 8 (render-out `bit_depth: 16`, ADR-1404).
//!
//! The renderer draws the display image into an `Rgba16Float` target ([`DeepTarget`]) through
//! [`crate::FrameRenderer::render_offscreen_deep`]. The post stack keeps its tonemapped image (and
//! the FXAA image) in `Rgba16Float` when its output target is a float format, so nothing on the way
//! is quantized to 8 bits. The values in the target are the same sRGB-encoded `0..=1` values an
//! `Rgba8Unorm` target stores, with half-float precision (11 significant bits; finer toward black).
//!
//! Readback goes in strips of rows ([`MAX_STRIP_BYTES`] at most per copy buffer) so a 4K frame
//! supersampled 3× (597 MB of half floats) never needs one huge mapped buffer. Each half float is
//! turned into a 16-bit code `round(clamp(v, 0, 1) · 65535)` by [`half_to_code`], an exact function
//! of the 16 input bits, so the result is the same on every platform.
//!
//! SMAA tiers (Mobile High, the Deck's non-temporal fallback) keep SMAA's 8-bit blend: on those
//! tiers a deep readout has 8-bit precision after the SMAA pass. The temporal tiers (TAA/TSR) and
//! FXAA are deep end to end.

use crate::{RenderDevice, RenderError};

/// Texture format of a deep target.
pub const DEEP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Largest readback buffer one strip copy uses, bytes.
pub const MAX_STRIP_BYTES: u64 = 64 << 20;

/// An `Rgba16Float` colour target the caller keeps between frames (a render-out job owns one at its
/// render size), so a long sequence does not allocate a large texture per frame.
#[derive(Clone)]
pub struct DeepTarget {
    /// The texture (`RENDER_ATTACHMENT | COPY_SRC | TEXTURE_BINDING`).
    pub texture: wgpu::Texture,
    /// Its view.
    pub view: wgpu::TextureView,
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
}

impl DeepTarget {
    /// Creates a `width`×`height` deep target (each side at least 1).
    #[must_use]
    pub fn new(device: &RenderDevice, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("deep readout target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEEP_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        crate::resource_inspector::track("deep readout target", &texture);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// True when this target is `width`×`height`.
    #[must_use]
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.width == width.max(1) && self.height == height.max(1)
    }
}

/// A deep frame read back: tightly packed RGBA, 16-bit sRGB-encoded codes (`0..=65535`), row by
/// row, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepFrame {
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
    /// `width * height * 4` codes.
    pub rgba16: Vec<u16>,
}

/// The format the post stack keeps its display-encoded image in (the tonemap output and the FXAA
/// image) for an output target of `out_format`: `Rgba16Float` for a float output without the HDR
/// display stage (a deep readout), `Rgba8Unorm` otherwise (unchanged behaviour).
#[must_use]
pub fn display_image_format(
    out_format: wgpu::TextureFormat,
    display_headroom: Option<f32>,
) -> wgpu::TextureFormat {
    let float_out = matches!(
        out_format,
        wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgba32Float
    );
    if float_out && display_headroom.is_none() {
        wgpu::TextureFormat::Rgba16Float
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    }
}

/// Exact IEEE half to single conversion (subnormals, infinities and NaN included).
#[must_use]
pub fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x3ff);
    let bits = match (exp, mant) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalize the mantissa.
            let shift = mant.leading_zeros() - 21;
            let mant = (mant << shift) & 0x3ff;
            sign | ((113 - shift) << 23) | (mant << 13)
        }
        (31, 0) => sign | 0x7f80_0000,
        (31, _) => sign | 0x7fc0_0000 | (mant << 13),
        _ => sign | ((exp + 112) << 23) | (mant << 13),
    };
    f32::from_bits(bits)
}

/// A half float's 16-bit display code: `round(clamp(v, 0, 1) · 65535)`; negative values and NaN
/// give 0, values above 1 give 65535.
#[must_use]
pub fn half_to_code(h: u16) -> u16 {
    let v = half_to_f32(h);
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= 1.0 {
        u16::MAX
    } else {
        // f64 keeps the product exact (11 significant bits times 16 bits).
        (f64::from(v) * 65535.0).round() as u16
    }
}

/// [`half_to_code`] for every half bit pattern, built once.
fn code_table() -> &'static [u16] {
    static TABLE: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| (0..=u16::MAX).map(half_to_code).collect())
}

/// Rows per strip for a row of `padded_row` bytes: as many as fit [`MAX_STRIP_BYTES`], at least one.
#[must_use]
pub fn strip_rows(padded_row: u64, height: u32) -> u32 {
    let rows = (MAX_STRIP_BYTES / padded_row.max(1)).max(1);
    rows.min(u64::from(height.max(1))) as u32
}

/// Reads a deep target back into 16-bit codes, in strips of [`strip_rows`] rows.
///
/// # Errors
///
/// The copy or the map failed ([`RenderError::Readback`], [`RenderError::Poll`]).
#[tracing::instrument(skip_all)]
pub fn read_deep(device: &RenderDevice, target: &DeepTarget) -> Result<DeepFrame, RenderError> {
    let (width, height) = (target.width, target.height);
    let row = width * 8;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let rows = strip_rows(u64::from(padded), height);
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("deep readback"),
        size: u64::from(padded) * u64::from(rows),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let table = code_table();
    let mut rgba16 = Vec::with_capacity(width as usize * height as usize * 4);
    let mut y0 = 0;
    while y0 < height {
        let strip = rows.min(height - y0);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("deep readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(strip),
                },
            },
            wgpu::Extent3d {
                width,
                height: strip,
                depth_or_array_layers: 1,
            },
        );
        crate::device::TrackedQueue::observe(&device.queue).submit([encoder.finish()]);
        let bytes = u64::from(padded) * u64::from(strip);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer
            .slice(..bytes)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        crate::device::TrackedDevice::observe(&device.device)
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        {
            let mapped = buffer
                .slice(..bytes)
                .get_mapped_range()
                .map_err(|e| RenderError::Readback(e.to_string()))?;
            for y in 0..strip as usize {
                let start = y * padded as usize;
                let (halves, _) = mapped[start..start + row as usize].as_chunks::<2>();
                rgba16.extend(halves.iter().map(|h| table[usize::from(u16::from_le_bytes(*h))]));
            }
        }
        buffer.unmap();
        y0 += strip;
    }
    Ok(DeepFrame {
        width,
        height,
        rgba16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_convert_exactly() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0x3800), 0.5);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x7bff), 65504.0);
        // Smallest subnormal and the largest one.
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        assert_eq!(half_to_f32(0x03ff), 1023.0 * 2f32.powi(-24));
        assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
        assert!(half_to_f32(0x7e00).is_nan());
        // Every finite half round-trips through f32 to the same value the textbook formula gives.
        for h in 0..=u16::MAX {
            let exp = i32::from((h >> 10) & 0x1f);
            if exp == 31 {
                continue;
            }
            let mant = f64::from(h & 0x3ff);
            let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
            let want = if exp == 0 {
                sign * mant * 2f64.powi(-24)
            } else {
                sign * (1.0 + mant / 1024.0) * 2f64.powi(exp - 15)
            };
            assert_eq!(f64::from(half_to_f32(h)), want, "{h:#06x}");
        }
    }

    #[test]
    fn codes_clamp_and_round() {
        assert_eq!(half_to_code(0x0000), 0);
        assert_eq!(half_to_code(0x8000), 0, "negative zero");
        assert_eq!(half_to_code(0xbc00), 0, "negative");
        assert_eq!(half_to_code(0x7e00), 0, "NaN");
        assert_eq!(half_to_code(0x3c00), 65535);
        assert_eq!(half_to_code(0x4000), 65535, "above one");
        assert_eq!(half_to_code(0x7c00), 65535, "infinity");
        assert_eq!(half_to_code(0x3800), 32768, "0.5 rounds half up");
        // Codes are monotone in the value over 0..=1 (halves 0x0000..=0x3c00 increase).
        let codes: Vec<u16> = (0..=0x3c00u16).map(half_to_code).collect();
        assert!(codes.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(code_table()[0x3800], 32768);
    }

    #[test]
    fn a_half_float_display_image_resolves_far_finer_than_8_bits() {
        // Between two adjacent 8-bit codes there are at least 8 distinct half-float codes across
        // the whole display range (the step is 2^-11 near white = 32 of 65535, 1/8 of 257).
        for code8 in 0..255u32 {
            let lo = code8 * 257;
            let hi = (code8 + 1) * 257;
            let distinct = (0..=0x3c00u16)
                .map(half_to_code)
                .filter(|c| (lo..hi).contains(&u32::from(*c)))
                .count();
            assert!(distinct >= 8, "8-bit code {code8}: {distinct} half codes");
        }
    }

    #[test]
    fn the_post_stack_goes_deep_only_for_a_float_output_without_the_hdr_display_stage() {
        use wgpu::TextureFormat as F;
        assert_eq!(display_image_format(F::Rgba16Float, None), F::Rgba16Float);
        assert_eq!(display_image_format(F::Rgba32Float, None), F::Rgba16Float);
        assert_eq!(
            display_image_format(F::Rgba16Float, Some(4.0)),
            F::Rgba8Unorm,
            "HDR swapchain unchanged"
        );
        for format in [
            F::Rgba8Unorm,
            F::Rgba8UnormSrgb,
            F::Bgra8UnormSrgb,
            F::Rgb10a2Unorm,
        ] {
            assert_eq!(display_image_format(format, None), F::Rgba8Unorm, "{format:?}");
        }
    }

    #[test]
    fn strips_bound_the_readback_buffer() {
        // 11520 px wide (4K at supersample 3): 92160 bytes a row; 728 rows a strip, 9 strips.
        let padded = 11_520u64 * 8;
        let rows = strip_rows(padded, 6480);
        assert!(u64::from(rows) * padded <= MAX_STRIP_BYTES);
        assert_eq!(rows, 728);
        assert_eq!(strip_rows(padded, 100), 100, "a short frame is one strip");
        assert_eq!(strip_rows(MAX_STRIP_BYTES * 2, 10), 1, "at least one row");
        assert_eq!(strip_rows(0, 0), 1);
    }
}
