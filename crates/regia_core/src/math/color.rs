// SPDX-License-Identifier: MPL-2.0
//! Linear RGBA colour with exact sRGB conversions.

use glam::{Vec3, Vec4};
use serde::{Deserialize, Serialize};

/// A colour in **linear** RGB with straight (not premultiplied) alpha, as the renderer consumes it.
///
/// Colours authored in sRGB (colour pickers, hex codes, 8-bit textures) are converted with the
/// sRGB transfer functions of IEC 61966-2-1 ([`Color::from_srgb_u8`], [`Color::from_hex`]).
/// Alpha is never gamma-encoded.
#[derive(
    Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, bytemuck::Pod, bytemuck::Zeroable,
)]
#[repr(C)]
pub struct Color {
    /// Red, linear.
    pub r: f32,
    /// Green, linear.
    pub g: f32,
    /// Blue, linear.
    pub b: f32,
    /// Alpha (coverage), 0 = transparent.
    pub a: f32,
}

/// Errors from [`Color::from_hex`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ColorParseError {
    /// Not 6 or 8 hex digits (after an optional `#`).
    #[error("color {0:?} is not #RRGGBB or #RRGGBBAA")]
    Length(String),
    /// A character is not a hex digit.
    #[error("color {0:?} contains a non-hex digit")]
    Digit(String),
}

/// sRGB-encoded value in `[0, 1]` → linear (IEC 61966-2-1).
#[inline]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear value in `[0, 1]` → sRGB-encoded (IEC 61966-2-1).
#[inline]
pub fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

#[inline]
fn quantize(v: f32) -> u8 {
    // Clamped and rounded, so the cast never truncates or saturates unexpectedly.
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl Color {
    /// Opaque white.
    pub const WHITE: Self = Self::rgb(1.0, 1.0, 1.0);
    /// Opaque black.
    pub const BLACK: Self = Self::rgb(0.0, 0.0, 0.0);
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);
    /// Opaque red.
    pub const RED: Self = Self::rgb(1.0, 0.0, 0.0);
    /// Opaque green.
    pub const GREEN: Self = Self::rgb(0.0, 1.0, 0.0);
    /// Opaque blue.
    pub const BLUE: Self = Self::rgb(0.0, 0.0, 1.0);
    /// Opaque yellow.
    pub const YELLOW: Self = Self::rgb(1.0, 1.0, 0.0);
    /// Opaque cyan.
    pub const CYAN: Self = Self::rgb(0.0, 1.0, 1.0);
    /// Opaque magenta.
    pub const MAGENTA: Self = Self::rgb(1.0, 0.0, 1.0);
    /// Opaque mid grey: sRGB 128 (linear ≈ 0.2158), the grey that looks halfway between black and white.
    pub const GRAY: Self = Self::rgb(0.215_860_53, 0.215_860_53, 0.215_860_53);

    /// Creates a colour from linear components.
    #[inline]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Creates an opaque colour from linear components.
    #[inline]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self::new(r, g, b, 1.0)
    }

    /// Creates a colour from sRGB-encoded components in `[0, 1]` (alpha linear).
    pub fn from_srgb(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self::new(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), a)
    }

    /// Creates a colour from 8-bit sRGB components (alpha is `a / 255`).
    pub fn from_srgb_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        let f = |v: u8| f32::from(v) / 255.0;
        Self::from_srgb(f(r), f(g), f(b), f(a))
    }

    /// sRGB-encoded components in `[0, 1]` (alpha unchanged, not clamped).
    pub fn to_srgb(&self) -> [f32; 4] {
        let f = |v: f32| linear_to_srgb(v.clamp(0.0, 1.0));
        [f(self.r), f(self.g), f(self.b), self.a]
    }

    /// 8-bit sRGB components, clamped and rounded. Inverse of [`Color::from_srgb_u8`] for every value.
    pub fn to_srgb_u8(&self) -> [u8; 4] {
        let [r, g, b, a] = self.to_srgb();
        [quantize(r), quantize(g), quantize(b), quantize(a)]
    }

    /// Parses `#RRGGBB` or `#RRGGBBAA` (sRGB, the `#` optional, case-insensitive).
    pub fn from_hex(hex: &str) -> Result<Self, ColorParseError> {
        let digits = hex.strip_prefix('#').unwrap_or(hex);
        if digits.len() != 6 && digits.len() != 8 {
            return Err(ColorParseError::Length(hex.to_owned()));
        }
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ColorParseError::Digit(hex.to_owned()));
        }
        let byte = |i: usize| {
            digits
                .get(i..i + 2)
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or_else(|| ColorParseError::Digit(hex.to_owned()))
        };
        let a = if digits.len() == 8 { byte(6)? } else { 255 };
        Ok(Self::from_srgb_u8(byte(0)?, byte(2)?, byte(4)?, a))
    }

    /// Formats as `#rrggbbaa` (sRGB, lowercase).
    pub fn to_hex(&self) -> String {
        let [r, g, b, a] = self.to_srgb_u8();
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }

    /// The same colour with alpha `a`.
    #[inline]
    pub const fn with_alpha(self, a: f32) -> Self {
        Self::new(self.r, self.g, self.b, a)
    }

    /// Components as `[r, g, b, a]`.
    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Components as a [`Vec4`].
    #[inline]
    pub const fn to_vec4(self) -> Vec4 {
        Vec4::new(self.r, self.g, self.b, self.a)
    }

    /// Colour channels as a [`Vec3`], dropping alpha.
    #[inline]
    pub const fn to_vec3(self) -> Vec3 {
        Vec3::new(self.r, self.g, self.b)
    }

    /// Straight → premultiplied alpha.
    #[inline]
    pub fn premultiplied(self) -> Self {
        Self::new(self.r * self.a, self.g * self.a, self.b * self.a, self.a)
    }

    /// Relative luminance of the linear colour (Rec. 709 / sRGB primaries).
    #[inline]
    pub fn luminance(&self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }
}

impl From<Vec4> for Color {
    fn from(v: Vec4) -> Self {
        Self::new(v.x, v.y, v.z, v.w)
    }
}

impl From<Color> for Vec4 {
    fn from(c: Color) -> Self {
        c.to_vec4()
    }
}

impl From<[f32; 4]> for Color {
    fn from([r, g, b, a]: [f32; 4]) -> Self {
        Self::new(r, g, b, a)
    }
}

impl From<Color> for [f32; 4] {
    fn from(c: Color) -> Self {
        c.to_array()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_srgb_byte_round_trips() {
        // Sweep each RGB axis independently with asymmetric values in the other channels.
        // Comparing all three returned channels also detects cross-channel contamination.
        for value in 0..=255u8 {
            for alpha in 0..=255u8 {
                for (rgb, channel) in [
                    ([value, 17, 231], 0),
                    ([19, value, 233], 1),
                    ([23, 239, value], 2),
                ] {
                    let color = Color::from_srgb_u8(rgb[0], rgb[1], rgb[2], alpha);
                    let round_trip = color.to_srgb_u8();
                    assert_eq!(
                        &round_trip[..3],
                        &rgb,
                        "channel {channel}, value {value}, alpha {alpha}"
                    );
                    assert_eq!(
                        round_trip[3], alpha,
                        "alpha {alpha}, channel {channel}, value {value}"
                    );
                    assert_eq!(color.a, f32::from(alpha) / 255.0, "linear alpha {alpha}");
                }
            }
        }
    }

    #[test]
    fn transfer_function_reference_values() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        // Below the linear-segment threshold the curve is c / 12.92.
        assert!((srgb_to_linear(0.04) - 0.04 / 12.92).abs() < 1e-9);
        // sRGB 0.5 → linear 0.21404114 (exact value of ((0.5 + 0.055) / 1.055)^2.4).
        assert!((srgb_to_linear(0.5) - 0.214_041_14).abs() < 1e-6);
        assert!((linear_to_srgb(0.214_041_14) - 0.5).abs() < 1e-6);
        assert!((Color::from_srgb_u8(128, 128, 128, 255).r - Color::GRAY.r).abs() < 1e-6);

        fn reference_srgb_to_linear(encoded: f64) -> f64 {
            if encoded <= 0.040_45 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        }
        fn reference_linear_to_srgb(linear: f64) -> f64 {
            if linear <= 0.003_130_8 {
                linear * 12.92
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            }
        }

        // The reference comparison allows six maximum f32 spacings over [0, 1]:
        // the decode's rounded addition and division contribute at most two spacings,
        // its power curve has slope at most 2.4, and one spacing covers powf rounding;
        // 6 * 2^-24 = 3.576_278_686_523_437_5e-7 absolute also covers the encode path.
        // For inverse round trips,
        // two f32 roundings propagated through the largest transfer slope are bounded
        // by 2 * 2^-24 * 12.92 ≈ 1.54e-6; round up to 2e-6.
        const REFERENCE_TOLERANCE: f64 = 3.576_278_686_523_437_5e-7;
        let decode_knot = 0.040_45_f32;
        let encode_knot = 0.003_130_8_f32;
        let mut decode_inputs = vec![0.0_f32, 1.0];
        let mut encode_inputs = vec![0.0_f32, 1.0];
        for knot in [decode_knot, encode_knot] {
            decode_inputs.extend([
                f32::from_bits(knot.to_bits() - 1),
                knot,
                f32::from_bits(knot.to_bits() + 1),
            ]);
            encode_inputs.extend([
                f32::from_bits(knot.to_bits() - 1),
                knot,
                f32::from_bits(knot.to_bits() + 1),
            ]);
        }
        for step in 0..=10_000 {
            let sample = step as f32 / 10_000.0;
            decode_inputs.push(sample);
            encode_inputs.push(sample);
        }
        decode_inputs.sort_by(f32::total_cmp);
        decode_inputs.dedup();
        encode_inputs.sort_by(f32::total_cmp);
        encode_inputs.dedup();

        let mut previous = -1.0_f32;
        for encoded in decode_inputs {
            let actual = srgb_to_linear(encoded);
            let reference = reference_srgb_to_linear(f64::from(encoded));
            assert!(
                (f64::from(actual) - reference).abs() <= REFERENCE_TOLERANCE,
                "decode {encoded:?}: {actual} vs {reference}"
            );
            assert!(actual >= previous, "decode decreased at {encoded:?}");
            previous = actual;
            assert!(
                (linear_to_srgb(actual) - encoded).abs() <= 2.0e-6,
                "decode/encode round trip {encoded:?}"
            );
        }
        previous = -1.0;
        let mut previous_reference = -1.0;
        let mut previous_linear = None;
        for linear in encode_inputs {
            let actual = linear_to_srgb(linear);
            let reference = reference_linear_to_srgb(f64::from(linear));
            assert!(
                (f64::from(actual) - reference).abs() <= REFERENCE_TOLERANCE,
                "encode {linear:?}: {actual} vs {reference}"
            );
            if actual < previous {
                // The published decimal cutoffs are rounded independently. The IEC
                // piecewise reference has a tiny seam at the first f32 above this knot;
                // retain that behavior and reject every other monotonicity regression.
                assert_eq!(previous_linear, Some(encode_knot));
                assert_eq!(linear.to_bits(), encode_knot.to_bits() + 1);
                let actual_step = f64::from(actual) - f64::from(previous);
                let reference_step = reference - previous_reference;
                assert!(
                    (actual_step - reference_step).abs() <= REFERENCE_TOLERANCE,
                    "encode seam step differs from f64 reference: {actual_step} vs {reference_step}"
                );
            }
            previous = actual;
            previous_reference = reference;
            previous_linear = Some(linear);
            assert!(
                (srgb_to_linear(actual) - linear).abs() <= 2.0e-6,
                "encode/decode round trip {linear:?}"
            );
        }
    }

    #[test]
    fn hex_parsing() {
        let c = Color::from_hex("#FF8000").expect("valid");
        assert_eq!(c.to_srgb_u8(), [255, 128, 0, 255]);
        assert_eq!(c.r, 1.0);
        assert_eq!(c.b, 0.0);
        assert_eq!(
            Color::from_hex("ff800040").expect("valid").to_srgb_u8(),
            [255, 128, 0, 64]
        );
        assert_eq!(Color::from_hex("#ffffff").expect("valid"), Color::WHITE);
        assert_eq!(Color::from_hex("#00000000").expect("valid"), Color::TRANSPARENT);
        assert!(matches!(Color::from_hex("#fff"), Err(ColorParseError::Length(_))));
        assert!(matches!(
            Color::from_hex("#ggffff"),
            Err(ColorParseError::Digit(_))
        ));
        assert!(matches!(
            Color::from_hex("#ffé0f"), /* 6 bytes, one non-ASCII */
            Err(ColorParseError::Digit(_))
        ));
        assert_eq!(c.to_hex(), "#ff8000ff");
        assert_eq!(Color::from_hex(&c.to_hex()).expect("valid"), c);

        for input in ["123456", "#123456", "abcdef80", "#ABCDEF80"] {
            assert!(Color::from_hex(input).is_ok(), "documented form {input:?}");
        }
        for ascii_count in [4, 6] {
            for position in 0..=ascii_count {
                let mut six_or_eight_bytes = "a".repeat(ascii_count);
                six_or_eight_bytes.insert(position, 'é');
                assert!(
                    matches!(
                        Color::from_hex(&six_or_eight_bytes),
                        Err(ColorParseError::Digit(_))
                    ),
                    "valid byte length with non-ASCII digit {six_or_eight_bytes:?}"
                );
            }
        }
        for input in ["é123", "é12345", "é1234567"] {
            assert!(
                matches!(Color::from_hex(input), Err(ColorParseError::Length(_))),
                "invalid byte length {input:?}"
            );
        }
        for digits in ["000000", "00000000"] {
            for index in 0..digits.len() {
                let mut invalid = digits.as_bytes().to_vec();
                invalid[index] = b'G';
                let invalid = String::from_utf8(invalid).expect("ASCII fixture");
                assert!(
                    matches!(Color::from_hex(&invalid), Err(ColorParseError::Digit(_))),
                    "invalid digit at byte {index}, form length {}",
                    digits.len()
                );
            }
        }
    }

    #[test]
    fn conversions_and_clamping() {
        assert_eq!(Color::new(2.0, -1.0, 0.0, 1.5).to_srgb_u8(), [255, 0, 0, 255]);
        let c = Color::new(0.1, 0.2, 0.3, 0.5);
        assert_eq!(Color::from(c.to_vec4()), c);
        assert_eq!(Color::from(c.to_array()), c);
        assert_eq!(c.premultiplied(), Color::new(0.05, 0.1, 0.15, 0.5));
        assert_eq!(c.with_alpha(1.0).a, 1.0);
        assert!((Color::WHITE.luminance() - 1.0).abs() < 1e-6);
        let json = serde_json::to_string(&c).expect("serialize");
        assert_eq!(serde_json::from_str::<Color>(&json).expect("deserialize"), c);
    }
}
